use na::RealField;
use slotmap::{Key, SlotMap};

use crate::pipeline::events::{ContactEvent, ContactEvents, ProximityEvent, ProximityEvents};
use crate::pipeline::narrow_phase::{
    ContactDispatcher, ProximityDispatcher, InteractionGraph, Interaction, CollisionObjectGraphIndex,
    ContactManifoldGenerator, ProximityDetector,
};
use crate::pipeline::world::{CollisionObjectHandle, CollisionObjectSlab, CollisionObject, GeometricQueryType};
use crate::query::{Proximity, ContactManifold, ContactId};
use crate::utils::SortedPair;

// FIXME: move this to the `narrow_phase` module.
/// Collision detector dispatcher for collision objects.
pub struct NarrowPhase<N: RealField> {
    contact_dispatcher: Box<ContactDispatcher<N>>,
    proximity_dispatcher: Box<ProximityDispatcher<N>>,
    contact_events: ContactEvents,
    proximity_events: ProximityEvents,
    id_allocator: SlotMap<ContactId, bool>,
}

impl<N: RealField> NarrowPhase<N> {
    /// Creates a new `NarrowPhase`.
    pub fn new(
        contact_dispatcher: Box<ContactDispatcher<N>>,
        proximity_dispatcher: Box<ProximityDispatcher<N>>,
    ) -> NarrowPhase<N>
    {
        NarrowPhase {
            contact_dispatcher,
            proximity_dispatcher,
            contact_events: ContactEvents::new(),
            proximity_events: ProximityEvents::new(),
            id_allocator: SlotMap::with_key(),
        }
    }

    fn garbage_collect_ids(&mut self, interactions: &mut InteractionGraph<N>) {
        for interaction in interactions.0.edge_weights_mut() {
            match interaction {
                Interaction::Contact(_, manifold) => {
                    for contact in manifold.contacts() {
                        if !contact.id.is_null() {
                            self.id_allocator[contact.id] = true;
                        }
                    }
                },
                Interaction::Proximity(_) => {}
            }
        }

        self.id_allocator.retain(|_, is_valid| {
            std::mem::replace(is_valid, false)
        })
    }


    /// Update the specified contact manifold between two collision objects.
    pub fn update_contact<T>(
        &mut self,
        co1: &CollisionObject<N, T>,
        co2: &CollisionObject<N, T>,
        detector: &mut ContactManifoldGenerator<N>,
        manifold: &mut ContactManifold<N>) {
        let had_contacts = manifold.len() != 0;

        if let Some(prediction) = co1
            .query_type()
            .contact_queries_to_prediction(co2.query_type())
        {
            manifold.save_cache_and_clear();
            let _ = detector.generate_contacts(
                &*self.contact_dispatcher,
                &co1.position(),
                co1.shape().as_ref(),
                None,
                &co2.position(),
                co2.shape().as_ref(),
                None,
                &prediction,
                manifold,
            );

            for contact in manifold.contacts_mut() {
                if contact.id.is_null() {
                    contact.id = self.id_allocator.insert(false)
                }
            }
        } else {
            panic!("Unable to compute contact between collision objects with query types different from `GeometricQueryType::Contacts(..)`.")
        }

        if manifold.len() == 0 {
            if had_contacts {
                self.contact_events.push(ContactEvent::Stopped(co1.handle(), co2.handle()));
            }
        } else {
            if !had_contacts {
                self.contact_events.push(ContactEvent::Started(co1.handle(), co2.handle()));
            }
        }
    }

    /// Update the specified proximity between two collision objects.
    pub fn update_proximity<T>(
        &mut self,
        co1: &CollisionObject<N, T>,
        co2: &CollisionObject<N, T>,
        detector: &mut ProximityDetector<N>) {
        let prev_prox = detector.proximity();

        let _ = detector.update(
            &*self.proximity_dispatcher,
            &co1.position(),
            co1.shape().as_ref(),
            &co2.position(),
            co2.shape().as_ref(),
            co1.query_type().query_limit() + co2.query_type().query_limit(),
        );

        let new_prox = detector.proximity();

        if new_prox != prev_prox {
            self.proximity_events.push(ProximityEvent::new(
                co1.handle(),
                co2.handle(),
                prev_prox,
                new_prox,
            ));
        }
    }

    /// Update the specified interaction between two collision objects.
    pub fn update_interaction<T>(
        &mut self,
        co1: &CollisionObject<N, T>,
        co2: &CollisionObject<N, T>,
        interaction: &mut Interaction<N>) {
        match interaction {
            Interaction::Contact(detector, manifold) => {
                self.update_contact(co1, co2, &mut **detector, manifold)
            }
            Interaction::Proximity(detector) => {
                self.update_proximity(co1, co2, &mut **detector)
            }
        }
    }

    /// Updates the narrow-phase by actually computing contact points and proximities between the
    /// interactions pairs reported by the broad-phase.
    ///
    /// This will push relevant events to `contact_events` and `proximity_events`.
    pub fn update<T>(&mut self, interactions: &mut InteractionGraph<N>, objects: &CollisionObjectSlab<N, T>, timestamp: usize, )
    {
        for eid in interactions.0.edge_indices() {
            let (id1, id2) = interactions.0.edge_endpoints(eid).unwrap();
            let co1 = &objects[interactions.0[id1]];
            let co2 = &objects[interactions.0[id2]];

            if co1.timestamp == timestamp || co2.timestamp == timestamp {
                self.update_interaction(co1, co2, interactions.0.edge_weight_mut(eid).unwrap())
            }
        }

        self.garbage_collect_ids(interactions);
    }

    /// Handles a pair of collision objects detected as either started or stopped interacting.
    pub fn handle_interaction<T>(
        &mut self,
        interactions: &mut InteractionGraph<N>,
        objects: &CollisionObjectSlab<N, T>,
        handle1: CollisionObjectHandle,
        handle2: CollisionObjectHandle,
        started: bool,
    )
    {
        let key = SortedPair::new(handle1, handle2);
        let co1 = &objects[key.0];
        let co2 = &objects[key.1];
        let id1 = co1.graph_index();
        let id2 = co2.graph_index();

        if started {
            if !interactions.0.contains_edge(id1, id2) {
                match (co1.query_type(), co2.query_type()) {
                    (GeometricQueryType::Contacts(..), GeometricQueryType::Contacts(..)) => {
                        let dispatcher = &self.contact_dispatcher;

                        if let Some(detector) = dispatcher
                            .get_contact_algorithm(co1.shape().as_ref(), co2.shape().as_ref())
                            {
                                let manifold = detector.init_manifold();
                                let _ = interactions.0.add_edge(id1, id2, Interaction::Contact(detector, manifold));
                            }
                    }
                    (_, GeometricQueryType::Proximity(_)) | (GeometricQueryType::Proximity(_), _) => {
                        let dispatcher = &self.proximity_dispatcher;

                        if let Some(detector) = dispatcher
                            .get_proximity_algorithm(co1.shape().as_ref(), co2.shape().as_ref())
                            {
                                let _ = interactions.0.add_edge(id1, id2, Interaction::Proximity(detector));
                            }
                    }
                }
            }
        } else {
            if let Some(eid) = interactions.0.find_edge(id1, id2) {
                if let Some(detector) = interactions.0.remove_edge(eid) {
                    match detector {
                        Interaction::Contact(_, mut manifold) => {
                            // Register a collision lost event if there was a contact.
                            if manifold.len() != 0 {
                                self.contact_events.push(ContactEvent::Stopped(co1.handle(), co2.handle()));
                            }

                            manifold.clear();
                        }
                        Interaction::Proximity(detector) => {
                            // Register a proximity lost signal if they were not disjoint.
                            let prev_prox = detector.proximity();

                            if prev_prox != Proximity::Disjoint {
                                let event = ProximityEvent::new(
                                    co1.handle(),
                                    co2.handle(),
                                    prev_prox,
                                    Proximity::Disjoint,
                                );
                                self.proximity_events.push(event);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handles the addition of a new collision object.
    pub fn handle_collision_object_added(
        &mut self,
        interactions: &mut InteractionGraph<N>,
        object: CollisionObjectHandle
    ) -> CollisionObjectGraphIndex {
        interactions.0.add_node(object)
    }

    /// Handles the removal of a collision object.
    pub fn handle_collision_object_removed<T>(
        &mut self,
        interactions: &mut InteractionGraph<N>,
        object: &CollisionObject<N, T>
    ) -> Option<CollisionObjectHandle> {
        let id = object.graph_index();
        let mut nbhs = interactions.0.neighbors(id).detach();

        // Clear all the manifold to avoid leaking contact IDs.
        while let Some((eid, _)) = nbhs.next(&interactions.0) {
            match interactions.0.edge_weight_mut(eid).unwrap() {
                Interaction::Contact(_, manifold) => manifold.clear(),
                Interaction::Proximity(_) => {}
            }
        }

        interactions.0.remove_node(object.graph_index())
    }

    /// The set of contact events generated by this narrow-phase.
    pub fn contact_events(&self) -> &ContactEvents {
        &self.contact_events
    }

    /// The set of proximity events generated by this narrow-phase.
    pub fn proximity_events(&self) -> &ProximityEvents {
        &self.proximity_events
    }

    /// Clear the events generated by this narrow-phase.
    pub fn clear_events(&mut self) {
        self.contact_events.clear();
        self.proximity_events.clear();
    }
}
