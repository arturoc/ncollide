use crate::bounding_volume::{self, BoundingVolume, AABB};
use crate::math::{Isometry, Point};
use na::RealField;
use crate::pipeline::broad_phase::{
    BroadPhase, BroadPhasePairFilter, BroadPhasePairFilters, DBVTBroadPhase, BroadPhaseProxyHandle,
    BroadPhaseInterferenceHandler
};
use crate::pipeline::events::{ContactEvent, ContactEvents, ProximityEvents};
use crate::pipeline::narrow_phase::{
    DefaultContactDispatcher, NarrowPhase, DefaultProximityDispatcher,
    CollisionObjectGraphIndex, Interaction, ContactAlgorithm, ProximityAlgorithm,
    TemporaryInteractionIndex,
};
use crate::pipeline::world::{
    CollisionGroups, CollisionGroupsPairFilter, CollisionObject, CollisionObjectHandle,
    CollisionObjectSlab, CollisionObjects, GeometricQueryType,
};
use crate::pipeline::narrow_phase::InteractionGraph;
use crate::query::{PointQuery, Ray, RayCast, RayIntersection, ContactManifold};
use crate::shape::ShapeHandle;
use std::vec::IntoIter;

/// Type of the broad phase trait-object used by the collision world.
pub type BroadPhaseObject<N> = Box<BroadPhase<N, AABB<N>, CollisionObjectHandle>>;


/// A world that handles collision objects.
pub struct CollisionWorld<N: RealField, T> {
    /// The set of objects on this collision world.
    pub objects: CollisionObjectSlab<N, T>,
    /// The broad phase used by this collision world.
    pub broad_phase: BroadPhaseObject<N>,
    /// The narrow-phase used by this collision world.
    pub narrow_phase: NarrowPhase<N>,
    /// The graph of interactions detected so far.
    pub interactions: InteractionGraph<N>,
    pair_filters: BroadPhasePairFilters<N, T>,
    timestamp: usize, // FIXME: allow modification of the other properties too.
}

struct CollisionWorldInterferenceHandler<'a, N: RealField, T: 'a> {
    narrow_phase: &'a mut NarrowPhase<N>,
    interactions: &'a mut InteractionGraph<N>,
    objects: &'a CollisionObjectSlab<N, T>,
    pair_filters: &'a BroadPhasePairFilters<N, T>,
}

impl <'a, N: RealField, T> BroadPhaseInterferenceHandler<CollisionObjectHandle> for CollisionWorldInterferenceHandler<'a, N, T> {
    fn is_interference_allowed(&mut self, b1: &CollisionObjectHandle, b2: &CollisionObjectHandle) -> bool {
        CollisionWorld::filter_collision(&self.pair_filters, &self.objects, *b1, *b2)
    }

    fn interference_started(&mut self, b1: &CollisionObjectHandle, b2: &CollisionObjectHandle) {
        self.narrow_phase.handle_interaction(
            self.interactions,
            &self.objects,
            *b1, *b2,
            true
        )
    }

    fn interference_stopped(&mut self, b1: &CollisionObjectHandle, b2: &CollisionObjectHandle) {
        self.narrow_phase.handle_interaction(
            &mut self.interactions,
            &self.objects,
            *b1, *b2,
            false
        )
    }
}

impl<N: RealField, T> CollisionWorld<N, T> {
    /// Creates a new collision world.
    // FIXME: use default values for `margin` and allow its modification by the user ?
    pub fn new(margin: N) -> CollisionWorld<N, T> {
        let objects = CollisionObjectSlab::new();
        let coll_dispatcher = Box::new(DefaultContactDispatcher::new());
        let prox_dispatcher = Box::new(DefaultProximityDispatcher::new());
        let broad_phase = Box::new(DBVTBroadPhase::<N, AABB<N>, CollisionObjectHandle>::new(
            margin,
        ));
        let narrow_phase = NarrowPhase::new(coll_dispatcher, prox_dispatcher);

        CollisionWorld {
            interactions: InteractionGraph::new(),
            objects,
            broad_phase,
            narrow_phase,
            pair_filters: BroadPhasePairFilters::new(),
            timestamp: 0,
        }
    }

    /// Adds a collision object to the world.
    pub fn add(
        &mut self,
        position: Isometry<N>,
        shape: ShapeHandle<N>,
        collision_groups: CollisionGroups,
        query_type: GeometricQueryType<N>,
        data: T,
    ) -> &mut CollisionObject<N, T>
    {
        let mut co = CollisionObject::new(
            CollisionObjectHandle::invalid(),
            BroadPhaseProxyHandle::invalid(),
            CollisionObjectGraphIndex::new(0),
            position,
            shape,
            collision_groups,
            query_type,
            data,
        );
        co.timestamp = self.timestamp;
        let handle = self.objects.insert(co);

        // Add objects.
        let co = &mut self.objects[handle];
        let mut aabb = bounding_volume::aabb(co.shape().as_ref(), co.position());
        aabb.loosen(co.query_type().query_limit());
        let proxy_handle = self.broad_phase.create_proxy(aabb, handle);
        let graph_index = self.narrow_phase.handle_collision_object_added(&mut self.interactions, handle);

        co.set_handle(handle);
        co.set_proxy_handle(proxy_handle);
        co.set_graph_index(graph_index);
        co
    }

    /// Updates the collision world.
    ///
    /// This executes the whole collision detection pipeline:
    /// 1. Clears the event pools.
    /// 2. Executes the broad phase first.
    /// 3. Executes the narrow phase.
    pub fn update(&mut self) {
        self.clear_events();
        self.perform_broad_phase();
        self.perform_narrow_phase();
    }

    /// Empty the contact and proximity event pools.
    pub fn clear_events(&mut self) {
        self.narrow_phase.clear_events();
    }

    /// Removed the specified set of collision objects from the world.
    ///
    /// Panics of any handle is invalid, or if the list contains duplicates.
    pub fn remove(&mut self, handles: &[CollisionObjectHandle]) {
        {
            let mut proxy_handles = Vec::new();

            for handle in handles {
                let co = self
                    .objects
                    .get(*handle)
                    .expect("Removal: collision object not found.");
                let graph_index = co.graph_index();
                proxy_handles.push(co.proxy_handle());

                if let Some(handle2) = self.narrow_phase.handle_collision_object_removed(&mut self.interactions, co) {
                    // Properly transfer the graph index.
                    self.objects[handle2].set_graph_index(graph_index)
                }
            }

            // NOTE: no need to notify the narrow phase in the callback because
            // the nodes have already been removed in the loop above.
            self.broad_phase.remove(&proxy_handles, &mut |_, _| {});
        }

        for handle in handles {
            let _ = self.objects.remove(*handle);
        }
    }

    /// Sets the position of the collision object attached to the specified object.
    pub fn set_position(&mut self, handle: CollisionObjectHandle, pos: Isometry<N>) {
        let co = self
            .objects
            .get_mut(handle)
            .expect("Set position: collision object not found.");
        co.set_position(pos.clone());
        co.timestamp = self.timestamp;
        let mut aabb = bounding_volume::aabb(co.shape().as_ref(), &pos);
        aabb.loosen(co.query_type().query_limit());
        self.broad_phase
            .deferred_set_bounding_volume(co.proxy_handle(), aabb);
    }

    /// Sets the position of the collision object attached to the specified object and update its bounding volume
    /// by taking into account its predicted next position.
    pub fn set_position_with_prediction(&mut self, handle: CollisionObjectHandle, pos: Isometry<N>, predicted_pos: &Isometry<N>) {
        let co = self
            .objects
            .get_mut(handle)
            .expect("Set position: collision object not found.");
        co.set_position(pos.clone());
        co.timestamp = self.timestamp;
        let mut aabb1 = bounding_volume::aabb(co.shape().as_ref(), &pos);
        let mut aabb2 = bounding_volume::aabb(co.shape().as_ref(), predicted_pos);
        aabb1.loosen(co.query_type().query_limit());
        aabb2.loosen(co.query_type().query_limit());
        aabb1.merge(&aabb2);
        self.broad_phase
            .deferred_set_bounding_volume(co.proxy_handle(), aabb1);

    }

    /// Sets the `GeometricQueryType` of the collision object.
    #[inline]
    pub fn set_query_type(&mut self, handle: CollisionObjectHandle, query_type: GeometricQueryType<N>) {
        let co = self
            .objects
            .get_mut(handle)
            .expect("Set query type: collision object not found.");
        co.set_query_type(query_type);
        self.broad_phase.deferred_recompute_all_proximities_with(co.proxy_handle());
    }

    /// Sets the shape of the given collision object.
    #[inline]
    pub fn set_shape(&mut self, handle: CollisionObjectHandle, shape: ShapeHandle<N>) {
        if let Some(co) = self.objects.get_mut(handle) {
            co.set_shape(shape);

            let mut aabb = bounding_volume::aabb(co.shape().as_ref(), co.position());

            aabb.loosen(co.query_type().query_limit());

            self.broad_phase.deferred_set_bounding_volume(co.proxy_handle(), aabb);
            self.broad_phase.deferred_recompute_all_proximities_with(co.proxy_handle());
        }
    }

    /// Apply the given deformations to the specified object.
    pub fn set_deformations(
        &mut self,
        handle: CollisionObjectHandle,
        coords: &[N],
    )
    {
        let co = self
            .objects
            .get_mut(handle)
            .expect("Set deformations: collision object not found.");
        co.set_deformations(coords);
        co.timestamp = self.timestamp;
        let mut aabb = bounding_volume::aabb(co.shape().as_ref(), co.position());
        aabb.loosen(co.query_type().query_limit());
        self.broad_phase
            .deferred_set_bounding_volume(co.proxy_handle(), aabb);
    }

    /// Adds a filter that tells if a potential collision pair should be ignored or not.
    ///
    /// The proximity filter returns `false` for a given pair of collision objects if they should
    /// be ignored by the narrow phase. Keep in mind that modifying the proximity filter will have
    /// a non-trivial overhead during the next update as it will force re-detection of all
    /// collision pairs.
    pub fn register_broad_phase_pair_filter<F>(&mut self, name: &str, filter: F)
    where F: BroadPhasePairFilter<N, T> {
        self.pair_filters
            .register_collision_filter(name, Box::new(filter));
        self.broad_phase.deferred_recompute_all_proximities();
    }

    /// Removes the pair filter named `name`.
    pub fn unregister_broad_phase_pair_filter(&mut self, name: &str) {
        if self.pair_filters.unregister_collision_filter(name) {
            self.broad_phase.deferred_recompute_all_proximities();
        }
    }

    /// Executes the broad phase of the collision detection pipeline.
    pub fn perform_broad_phase(&mut self) {
        self.broad_phase.update(&mut CollisionWorldInterferenceHandler {
            interactions: &mut self.interactions,
            narrow_phase: &mut self.narrow_phase,
            pair_filters: &self.pair_filters,
            objects: &self.objects,
        });
    }

    /// Executes the narrow phase of the collision detection pipeline.
    pub fn perform_narrow_phase(&mut self) {
        self.narrow_phase.update(&mut self.interactions, &self.objects, self.timestamp);
        self.timestamp = self.timestamp + 1;
    }

    /// The broad-phase aabb for the given collision object.
    pub fn broad_phase_aabb(&self, handle: CollisionObjectHandle) -> Option<&AABB<N>> {
        let co = self.objects.get(handle)?;
        self.broad_phase.proxy(co.proxy_handle()).map(|p| p.0)
    }

    /// Iterates through all collision objects.
    #[inline]
    pub fn collision_objects(&self) -> CollisionObjects<N, T> {
        self.objects.iter()
    }

    /// Returns a reference to the collision object identified by its handle.
    #[inline]
    pub fn collision_object(
        &self,
        handle: CollisionObjectHandle,
    ) -> Option<&CollisionObject<N, T>>
    {
        self.objects.get(handle)
    }

    /// Returns a mutable reference to the collision object identified by its handle.
    #[inline]
    pub fn collision_object_mut(
        &mut self,
        handle: CollisionObjectHandle,
    ) -> Option<&mut CollisionObject<N, T>>
    {
        self.objects.get_mut(handle)
    }

    /// Returns a mutable reference to a pair collision object identified by their handles.
    ///
    /// Panics if both handles are equal.
    #[inline]
    pub fn collision_object_pair_mut(
        &mut self,
        handle1: CollisionObjectHandle,
        handle2: CollisionObjectHandle,
    ) -> (Option<&mut CollisionObject<N, T>>, Option<&mut CollisionObject<N, T>>)
    {
        self.objects.get_pair_mut(handle1, handle2)
    }

    /// Sets the collision groups of the given collision object.
    #[inline]
    pub fn set_collision_groups(&mut self, handle: CollisionObjectHandle, groups: CollisionGroups) {
        if let Some(co) = self.objects.get_mut(handle) {
            co.set_collision_groups(groups);
            self.broad_phase
                .deferred_recompute_all_proximities_with(co.proxy_handle());
        }
    }

    /// Computes the interferences between every rigid bodies on this world and a ray.
    #[inline]
    pub fn interferences_with_ray<'a, 'b>(
        &'a self,
        ray: &'b Ray<N>,
        groups: &'b CollisionGroups,
    ) -> InterferencesWithRay<'a, 'b, N, T>
    {
        // FIXME: avoid allocation.
        let mut handles = Vec::new();
        self.broad_phase.interferences_with_ray(ray, &mut handles);

        InterferencesWithRay {
            ray,
            groups,
            objects: &self.objects,
            handles: handles.into_iter(),
        }
    }

    /// Computes the interferences between every rigid bodies of a given broad phase, and a point.
    #[inline]
    pub fn interferences_with_point<'a, 'b>(
        &'a self,
        point: &'b Point<N>,
        groups: &'b CollisionGroups,
    ) -> InterferencesWithPoint<'a, 'b, N, T>
    {
        // FIXME: avoid allocation.
        let mut handles = Vec::new();
        self.broad_phase
            .interferences_with_point(point, &mut handles);

        InterferencesWithPoint {
            point: point,
            groups: groups,
            objects: &self.objects,
            handles: handles.into_iter(),
        }
    }

    /// Computes the interferences between every rigid bodies of a given broad phase, and a aabb.
    #[inline]
    pub fn interferences_with_aabb<'a, 'b>(
        &'a self,
        aabb: &'b AABB<N>,
        groups: &'b CollisionGroups,
    ) -> InterferencesWithAABB<'a, 'b, N, T>
    {
        // FIXME: avoid allocation.
        let mut handles = Vec::new();
        self.broad_phase
            .interferences_with_bounding_volume(aabb, &mut handles);

        InterferencesWithAABB {
            groups: groups,
            objects: &self.objects,
            handles: handles.into_iter(),
        }
    }

    /// Customize the selection of narrowphase collision detection algorithms
    pub fn set_narrow_phase(&mut self, narrow_phase: NarrowPhase<N>) {
        self.narrow_phase = narrow_phase;
        self.broad_phase.deferred_recompute_all_proximities();
    }

    /*
     *
     * Operations on the interaction graph.
     *
     */

    /// All the potential interactions pairs.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn interaction_pairs(&self, effective_only: bool) -> impl Iterator<Item = (
        CollisionObjectHandle,
        CollisionObjectHandle,
        &Interaction<N>
    )> {
        self.interactions.interaction_pairs(effective_only)
    }

    /// All the potential contact pairs.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn contact_pairs(&self, effective_only: bool) -> impl Iterator<Item = (
        CollisionObjectHandle,
        CollisionObjectHandle,
        &ContactAlgorithm<N>,
        &ContactManifold<N>,
    )> {
        self.interactions.contact_pairs(effective_only)
    }

    /// All the potential proximity pairs.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn proximity_pairs(&self, effective_only: bool) -> impl Iterator<Item = (
        CollisionObjectHandle,
        CollisionObjectHandle,
        &ProximityAlgorithm<N>,
    )> {
        self.interactions.proximity_pairs(effective_only)
    }

    /// The potential interaction pair between the two specified collision objects.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn interaction_pair(&self, handle1: CollisionObjectHandle, handle2: CollisionObjectHandle, effective_only: bool)
        -> Option<(CollisionObjectHandle, CollisionObjectHandle, &Interaction<N>)> {
        let co1 = self.objects.get(handle1)?;
        let co2 = self.objects.get(handle2)?;
        let id1 = co1.graph_index();
        let id2 = co2.graph_index();
        self.interactions.interaction_pair(id1, id2, effective_only)
    }

    /// The potential contact pair between the two specified collision objects.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn contact_pair(&self, handle1: CollisionObjectHandle, handle2: CollisionObjectHandle, effective_only: bool)
        -> Option<(CollisionObjectHandle, CollisionObjectHandle, &ContactAlgorithm<N>, &ContactManifold<N>)> {
        let co1 = self.objects.get(handle1)?;
        let co2 = self.objects.get(handle2)?;
        let id1 = co1.graph_index();
        let id2 = co2.graph_index();
        self.interactions.contact_pair(id1, id2, effective_only)
    }


    /// The potential proximity pair between the two specified collision objects.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn proximity_pair(&self, handle1: CollisionObjectHandle, handle2: CollisionObjectHandle, effective_only: bool)
        -> Option<(CollisionObjectHandle, CollisionObjectHandle, &ProximityAlgorithm<N>)> {
        let co1 = self.objects.get(handle1)?;
        let co2 = self.objects.get(handle2)?;
        let id1 = co1.graph_index();
        let id2 = co2.graph_index();
        self.interactions.proximity_pair(id1, id2, effective_only)
    }

    /// All the interaction pairs involving the specified collision object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn interactions_with(&self, handle: CollisionObjectHandle, effective_only: bool)
        -> Option<impl Iterator<Item = (CollisionObjectHandle, CollisionObjectHandle, &Interaction<N>)>> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.interactions_with(id, effective_only))
    }

    /// All the mutable interactions pairs involving the specified collision object.
    ///
    /// This also returns a mutable reference to the narrow-phase which is necessary for updating the interaction if needed.
    /// For interactions between a collision object and itself, only one mutable reference to the collision object is returned.
    pub fn interactions_with_mut(&mut self, handle: CollisionObjectHandle)
        -> Option<(&mut NarrowPhase<N>, impl Iterator<Item = (CollisionObjectHandle, CollisionObjectHandle, TemporaryInteractionIndex, &mut Interaction<N>)>)> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some((&mut self.narrow_phase, self.interactions.interactions_with_mut(id)))
    }

    /// All the proximity pairs involving the specified collision object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn proximities_with(&self, handle: CollisionObjectHandle, effective_only: bool)
        -> Option<impl Iterator<Item = (CollisionObjectHandle, CollisionObjectHandle, &ProximityAlgorithm<N>)>> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.proximities_with(id, effective_only))
    }

    /// All the contact pairs involving the specified collision object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn contacts_with(&self, handle: CollisionObjectHandle, effective_only: bool)
        -> Option<impl Iterator<Item = (CollisionObjectHandle, CollisionObjectHandle, &ContactAlgorithm<N>, &ContactManifold<N>)>> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.contacts_with(id, effective_only))
    }

    /// All the collision object handles of collision objects interacting with the specified collision object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn collision_objects_interacting_with<'a>(&'a self, handle: CollisionObjectHandle)
        -> Option<impl Iterator<Item = CollisionObjectHandle> + 'a> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.collision_objects_interacting_with(id))
    }

    /// All the collision object handles of collision objects in potential contact with the specified collision
    /// object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn collision_objects_in_contact_with<'a>(&'a self, handle: CollisionObjectHandle)
        -> Option<impl Iterator<Item = CollisionObjectHandle> + 'a> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.collision_objects_in_contact_with(id))
    }


    /// All the collision object handles of collision objects in potential proximity of with the specified
    /// collision object.
    ///
    /// Refer to the official [user guide](https://nphysics.org/interaction_handling_and_sensors/#interaction-iterators)
    /// for details.
    pub fn collision_objects_in_proximity_of<'a>(&'a self, handle: CollisionObjectHandle)
        -> Option<impl Iterator<Item = CollisionObjectHandle> + 'a> {
        let co = self.objects.get(handle)?;
        let id = co.graph_index();
        Some(self.interactions.collision_objects_in_proximity_of(id))
    }


    /*
     *
     * Events
     *
     */
    /// The contact events pool.
    pub fn contact_events(&self) -> &ContactEvents {
        self.narrow_phase.contact_events()
    }

    /// The proximity events pool.
    pub fn proximity_events(&self) -> &ProximityEvents {
        self.narrow_phase.proximity_events()
    }

    // Filters by group and by the user-provided callback.
    #[inline]
    fn filter_collision(
        filters: &BroadPhasePairFilters<N, T>,
        objects: &CollisionObjectSlab<N, T>,
        handle1: CollisionObjectHandle,
        handle2: CollisionObjectHandle,
    ) -> bool
    {
        let o1 = &objects[handle1];
        let o2 = &objects[handle2];
        let filter_by_groups = CollisionGroupsPairFilter;

        filter_by_groups.is_pair_valid(o1, o2) && filters.is_pair_valid(o1, o2)
    }
}

/// Iterator through all the objects on the world that intersect a specific ray.
pub struct InterferencesWithRay<'a, 'b, N: 'a + RealField, T: 'a> {
    ray: &'b Ray<N>,
    objects: &'a CollisionObjectSlab<N, T>,
    groups: &'b CollisionGroups,
    handles: IntoIter<&'a CollisionObjectHandle>,
}

impl<'a, 'b, N: RealField, T> Iterator for InterferencesWithRay<'a, 'b, N, T> {
    type Item = (&'a CollisionObject<N, T>, RayIntersection<N>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(handle) = self.handles.next() {
            let co = &self.objects[*handle];

            if co.collision_groups().can_interact_with_groups(self.groups) {
                let inter = co
                    .shape()
                    .toi_and_normal_with_ray(&co.position(), self.ray, true);

                if let Some(inter) = inter {
                    return Some((co, inter));
                }
            }
        }

        None
    }
}

/// Iterator through all the objects on the world that intersect a specific point.
pub struct InterferencesWithPoint<'a, 'b, N: 'a + RealField, T: 'a> {
    point: &'b Point<N>,
    objects: &'a CollisionObjectSlab<N, T>,
    groups: &'b CollisionGroups,
    handles: IntoIter<&'a CollisionObjectHandle>,
}

impl<'a, 'b, N: RealField, T> Iterator for InterferencesWithPoint<'a, 'b, N, T> {
    type Item = &'a CollisionObject<N, T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(handle) = self.handles.next() {
            let co = &self.objects[*handle];

            if co.collision_groups().can_interact_with_groups(self.groups)
                && co.shape().contains_point(&co.position(), self.point)
            {
                return Some(co);
            }
        }

        None
    }
}

/// Iterator through all the objects on the world which bounding volume intersects a specific AABB.
pub struct InterferencesWithAABB<'a, 'b, N: 'a + RealField, T: 'a> {
    objects: &'a CollisionObjectSlab<N, T>,
    groups: &'b CollisionGroups,
    handles: IntoIter<&'a CollisionObjectHandle>,
}

impl<'a, 'b, N: RealField, T> Iterator for InterferencesWithAABB<'a, 'b, N, T> {
    type Item = &'a CollisionObject<N, T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(handle) = self.handles.next() {
            let co = &self.objects[*handle];

            if co.collision_groups().can_interact_with_groups(self.groups) {
                return Some(co);
            }
        }

        None
    }
}
