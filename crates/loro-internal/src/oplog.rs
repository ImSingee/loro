pub(crate) mod dag;

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use fxhash::FxHashMap;
use loro_common::HasId;
use rle::{HasLength, RleVec};
// use tabled::measurment::Percent;

use crate::change::{Change, Lamport, Timestamp};
use crate::container::list::list_op;
use crate::dag::DagUtils;
use crate::encoding::{decode_oplog, encode_oplog, EncodeMode};
use crate::encoding::{ClientChanges, RemoteClientChanges};
use crate::id::{Counter, PeerID, ID};
use crate::op::{RawOpContent, RemoteOp};
use crate::span::{HasCounterSpan, HasIdSpan, HasLamportSpan};
use crate::version::{Frontiers, ImVersionVector, VersionVector};
use crate::LoroError;

use super::arena::SharedArena;

/// [OpLog] store all the ops i.e. the history.
/// It allows multiple [AppState] to attach to it.
/// So you can derive different versions of the state from the [OpLog].
/// It allows us to build a version control system.
///
/// The causal graph should always be a DAG and complete. So we can always find the LCA.
/// If deps are missing, we can't import the change. It will be put into the `pending_changes`.
pub struct OpLog {
    pub(crate) dag: AppDag,
    pub(crate) arena: SharedArena,
    pub(crate) changes: ClientChanges,
    /// **lamport starts from 0**
    pub(crate) next_lamport: Lamport,
    pub(crate) latest_timestamp: Timestamp,
    /// Pending changes that haven't been applied to the dag.
    /// A change can be imported only when all its deps are already imported.
    /// Key is the ID of the missing dep
    pending_changes: FxHashMap<ID, Vec<Change>>,
}

/// [AppDag] maintains the causal graph of the app.
/// It's faster to answer the question like what's the LCA version
#[derive(Debug, Clone, Default)]
pub struct AppDag {
    map: FxHashMap<PeerID, RleVec<[AppDagNode; 1]>>,
    frontiers: Frontiers,
    vv: VersionVector,
}

#[derive(Debug, Clone)]
pub struct AppDagNode {
    peer: PeerID,
    cnt: Counter,
    lamport: Lamport,
    deps: Frontiers,
    vv: ImVersionVector,
    len: usize,
}

impl Clone for OpLog {
    fn clone(&self) -> Self {
        Self {
            dag: self.dag.clone(),
            arena: Default::default(),
            changes: self.changes.clone(),
            next_lamport: self.next_lamport,
            latest_timestamp: self.latest_timestamp,
            pending_changes: Default::default(),
        }
    }
}

impl std::fmt::Debug for OpLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpLog")
            .field("dag", &self.dag)
            .field("changes", &self.changes)
            .field("next_lamport", &self.next_lamport)
            .field("latest_timestamp", &self.latest_timestamp)
            .finish()
    }
}

impl OpLog {
    pub fn new() -> Self {
        Self {
            dag: AppDag::default(),
            arena: Default::default(),
            changes: ClientChanges::default(),
            next_lamport: 0,
            latest_timestamp: Timestamp::default(),
            pending_changes: Default::default(),
        }
    }

    pub fn new_with_arena(arena: SharedArena) -> Self {
        Self {
            dag: AppDag::default(),
            arena,
            next_lamport: 0,
            ..Default::default()
        }
    }

    pub fn latest_timestamp(&self) -> Timestamp {
        self.latest_timestamp
    }

    pub fn get_timestamp_of_version(&self, f: &Frontiers) -> Timestamp {
        let mut timestamp = Timestamp::default();
        for id in f.iter() {
            if let Some(change) = self.lookup_change(*id) {
                timestamp = timestamp.max(change.timestamp);
            }
        }

        timestamp
    }

    pub fn is_empty(&self) -> bool {
        self.dag.map.is_empty() && self.arena.is_empty()
    }

    /// Import a change.
    ///
    /// Pending changes that haven't been applied to the dag.
    /// A change can be imported only when all its deps are already imported.
    /// Key is the ID of the missing dep
    ///
    /// # Err
    ///
    /// - Return Err(LoroError::UsedOpID) when the change's id is occupied
    /// - Return Err(LoroError::DecodeError) when the change's deps are missing
    pub fn import_local_change(&mut self, change: Change) -> Result<(), LoroError> {
        self.check_id_is_not_duplicated(change.id)?;
        if let Err(id) = self.check_deps(&change.deps) {
            self.pending_changes.entry(id).or_default().push(change);
            return Err(LoroError::DecodeError(
                format!("Missing dep {:?}", id).into_boxed_str(),
            ));
        }

        if cfg!(debug_assertions) {
            let lamport = self.dag.frontiers_to_next_lamport(&change.deps);
            assert_eq!(
                lamport, change.lamport,
                "{:#?}\nDAG={:#?}",
                &change, &self.dag
            );
        }

        self.next_lamport = self.next_lamport.max(change.lamport_end());
        self.latest_timestamp = self.latest_timestamp.max(change.timestamp);
        self.dag.vv.extend_to_include_last_id(change.id_last());
        self.dag.frontiers.retain_non_included(&change.deps);
        self.dag.frontiers.filter_peer(change.id.peer);
        self.dag.frontiers.push(change.id_last());
        let len = change.content_len();
        if change.deps.len() == 1 && change.deps[0].peer == change.id.peer {
            // don't need to push new element to dag because it only depends on itself
            let nodes = self.dag.map.get_mut(&change.id.peer).unwrap();
            let last = nodes.vec_mut().last_mut().unwrap();
            assert_eq!(last.peer, change.id.peer);
            assert_eq!(last.cnt + last.len as Counter, change.id.counter);
            assert_eq!(last.lamport + last.len as Lamport, change.lamport);
            last.len = change.id.counter as usize + len - last.cnt as usize;
        } else {
            let vv = self.dag.frontiers_to_im_vv(&change.deps);
            self.dag
                .map
                .entry(change.id.peer)
                .or_default()
                .push(AppDagNode {
                    vv,
                    peer: change.id.peer,
                    cnt: change.id.counter,
                    lamport: change.lamport,
                    deps: change.deps.clone(),
                    len,
                });
        }
        self.changes.entry(change.id.peer).or_default().push(change);
        Ok(())
    }

    fn check_id_is_not_duplicated(&self, id: ID) -> Result<(), LoroError> {
        let cur_end = self.dag.vv.get(&id.peer).cloned().unwrap_or(0);
        if cur_end > id.counter {
            return Err(LoroError::UsedOpID { id });
        }

        Ok(())
    }

    fn check_deps(&self, deps: &Frontiers) -> Result<(), ID> {
        for dep in deps.iter() {
            if !self.dag.vv.includes_id(*dep) {
                return Err(*dep);
            }
        }

        Ok(())
    }

    pub fn next_lamport(&self) -> Lamport {
        self.next_lamport
    }

    pub fn next_id(&self, peer: PeerID) -> ID {
        let cnt = self.dag.vv.get(&peer).copied().unwrap_or(0);
        ID::new(peer, cnt)
    }

    pub fn get_peer_changes(&self, peer: PeerID) -> Option<&RleVec<[Change; 0]>> {
        self.changes.get(&peer)
    }

    pub(crate) fn vv(&self) -> &VersionVector {
        &self.dag.vv
    }

    pub(crate) fn frontiers(&self) -> &Frontiers {
        &self.dag.frontiers
    }

    /// - Ordering::Less means self is less than target or parallel
    /// - Ordering::Equal means versions equal
    /// - Ordering::Greater means self's version is greater than target
    pub fn cmp_frontiers(&self, other: &Frontiers) -> Ordering {
        self.dag.cmp_frontiers(other)
    }

    pub(crate) fn export_changes_from(&self, from: &VersionVector) -> RemoteClientChanges {
        let mut changes = RemoteClientChanges::default();
        for (&peer, &cnt) in self.vv().iter() {
            let start_cnt = from.get(&peer).copied().unwrap_or(0);
            if cnt <= start_cnt {
                continue;
            }

            let mut temp = Vec::new();
            if let Some(peer_changes) = self.changes.get(&peer) {
                if let Some(result) = peer_changes.get_by_atom_index(start_cnt) {
                    for change in &peer_changes.vec()[result.merged_index..] {
                        temp.push(self.convert_change_to_remote(change))
                    }
                }
            }

            if !temp.is_empty() {
                changes.insert(peer, temp);
            }
        }

        changes
    }

    pub(crate) fn get_change_since(&self, id: ID) -> Vec<Change> {
        let mut changes = Vec::new();
        if let Some(peer_changes) = self.changes.get(&id.peer) {
            if let Some(result) = peer_changes.get_by_atom_index(id.counter) {
                for change in &peer_changes.vec()[result.merged_index..] {
                    changes.push(change.clone())
                }
            }
        }

        changes
    }

    pub fn get_change_at(&self, id: ID) -> Option<Change> {
        if let Some(peer_changes) = self.changes.get(&id.peer) {
            if let Some(result) = peer_changes.get_by_atom_index(id.counter) {
                return Some(peer_changes.vec()[result.merged_index].clone());
            }
        }

        None
    }

    fn convert_change_to_remote(&self, change: &Change) -> Change<RemoteOp> {
        let mut ops = RleVec::new();
        for op in change.ops.iter() {
            let raw_op = self.local_op_to_remote(op);
            ops.push(raw_op);
        }

        Change {
            ops,
            id: change.id,
            deps: change.deps.clone(),
            lamport: change.lamport,
            timestamp: change.timestamp,
        }
    }

    pub(crate) fn local_op_to_remote(&self, op: &crate::op::Op) -> RemoteOp<'_> {
        let container = self.arena.get_container_id(op.container).unwrap();
        let mut contents = RleVec::new();
        match &op.content {
            crate::op::InnerContent::List(list) => match list {
                list_op::InnerListOp::Insert { slice, pos } => match container.container_type() {
                    loro_common::ContainerType::Text => {
                        let str = self
                            .arena
                            .slice_str(slice.0.start as usize..slice.0.end as usize);
                        contents.push(RawOpContent::List(list_op::ListOp::Insert {
                            slice: crate::container::text::text_content::ListSlice::RawStr {
                                unicode_len: str.chars().count(),
                                str: Cow::Owned(str),
                            },
                            pos: *pos,
                        }));
                    }
                    loro_common::ContainerType::List => {
                        contents.push(RawOpContent::List(list_op::ListOp::Insert {
                            slice: crate::container::text::text_content::ListSlice::RawData(
                                Cow::Owned(
                                    self.arena
                                        .get_values(slice.0.start as usize..slice.0.end as usize),
                                ),
                            ),
                            pos: *pos,
                        }))
                    }
                    loro_common::ContainerType::Map => unreachable!(),
                },
                list_op::InnerListOp::Delete(del) => {
                    contents.push(RawOpContent::List(list_op::ListOp::Delete(*del)))
                }
            },
            crate::op::InnerContent::Map(map) => {
                let value = self.arena.get_value(map.value as usize);
                contents.push(RawOpContent::Map(crate::container::map::MapSet {
                    key: map.key.clone(),
                    value: value.unwrap_or(crate::LoroValue::Null), // TODO: decide map delete value
                }))
            }
        };

        RemoteOp {
            container,
            contents,
            counter: op.counter,
        }
    }

    // Changes are expected to be sorted by counter in each value in the hashmap
    // They should also be contiuous  (TODO: check this)
    pub(crate) fn import_remote_changes(
        &mut self,
        changes: RemoteClientChanges,
    ) -> Result<(), LoroError> {
        // check whether we can append the new changes
        // TODO: support pending changes
        let vv = &self.dag.vv;
        for (peer, changes) in &changes {
            if changes.is_empty() {
                continue;
            }

            if let Some(end_cnt) = vv.get(peer) {
                let first_id = changes.first().unwrap().id_start();
                if first_id.counter > *end_cnt {
                    return Err(LoroError::DecodeError(
                        // TODO: Support pending changes to avoid this error
                        "Changes are not appliable yet."
                            .to_string()
                            .into_boxed_str(),
                    ));
                }
            }
        }

        // TODO: should we check deps here?
        let len = changes.iter().fold(0, |last, this| last + this.1.len());
        let mut change_causal_arr = Vec::with_capacity(len);
        // op_converter is faster than using arena directly
        self.arena.with_op_converter(|converter| {
            for (peer, changes) in changes {
                if changes.is_empty() {
                    continue;
                }

                let cur_end_cnt = self.changes.get(&peer).map(|x| x.atom_len()).unwrap_or(0);
                let last_change = changes.last().unwrap();
                self.dag.vv.extend_to_include_last_id(last_change.id_last());
                self.next_lamport = self.next_lamport.max(last_change.lamport_end());
                self.latest_timestamp = self.latest_timestamp.max(last_change.timestamp);
                for change in changes {
                    if change.id.counter < cur_end_cnt {
                        // truncate included changes
                        continue;
                    }

                    let mut ops = RleVec::new();
                    for op in change.ops {
                        for content in op.contents.into_iter() {
                            let op =
                                converter.convert_single_op(&op.container, op.counter, content);
                            ops.push(op);
                        }
                    }

                    let change = Change {
                        ops,
                        id: change.id,
                        deps: change.deps,
                        lamport: change.lamport,
                        timestamp: change.timestamp,
                    };
                    change_causal_arr.push(change);
                }
            }
        });

        // TODO: Perf
        change_causal_arr.sort_by_key(|x| x.lamport);
        // debug_dbg!(&change_causal_arr);
        for change in change_causal_arr {
            let len = change.content_len();
            if change.deps.len() == 1 && change.deps[0].peer == change.id.peer {
                // don't need to push new element to dag because it only depends on itself
                let nodes = self.dag.map.get_mut(&change.id.peer).unwrap();
                let last = nodes.vec_mut().last_mut().unwrap();
                assert_eq!(last.peer, change.id.peer);
                assert_eq!(last.cnt + last.len as Counter, change.id.counter);
                assert_eq!(last.lamport + last.len as Lamport, change.lamport);
                last.len = change.id.counter as usize + len - last.cnt as usize;
            } else {
                let vv = self.dag.frontiers_to_im_vv(&change.deps);
                self.dag
                    .map
                    .entry(change.id.peer)
                    .or_default()
                    .push(AppDagNode {
                        vv,
                        peer: change.id.peer,
                        cnt: change.id.counter,
                        lamport: change.lamport,
                        deps: change.deps.clone(),
                        len,
                    });
            }
            self.changes.entry(change.id.peer).or_default().push(change);
        }

        self.dag.frontiers = self.dag.vv_to_frontiers(&self.dag.vv);
        Ok(())
    }

    /// lookup change by id.
    ///
    /// if id does not included in this oplog, return None
    pub(crate) fn lookup_change(&self, id: ID) -> Option<&Change> {
        self.changes.get(&id.peer).and_then(|changes| {
            // Because get_by_atom_index would return Some if counter is at the end,
            // we cannot use it directly.
            // TODO: maybe we should refactor this
            if id.counter <= changes.last().unwrap().id_last().counter {
                Some(changes.get_by_atom_index(id.counter).unwrap().element)
            } else {
                None
            }
        })
    }

    #[allow(unused)]
    pub(crate) fn lookup_op(&self, id: ID) -> Option<&crate::op::Op> {
        self.lookup_change(id)
            .and_then(|change| change.ops.get_by_atom_index(id.counter).map(|x| x.element))
    }

    pub fn export_from(&self, vv: &VersionVector) -> Vec<u8> {
        encode_oplog(self, EncodeMode::Auto(vv.clone()))
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<(), LoroError> {
        decode_oplog(self, data)
    }

    /// Iterates over all changes between `a` and `b` peer by peer (not in causal order, fast)
    pub(crate) fn for_each_change_within(
        &self,
        a: &VersionVector,
        b: &VersionVector,
        mut f: impl FnMut(&Change),
    ) {
        for (peer, changes) in self.changes.iter() {
            let mut from_cnt = a.get(peer).copied().unwrap_or(0);
            let mut to_cnt = b.get(peer).copied().unwrap_or(0);
            if from_cnt == to_cnt {
                continue;
            }

            if to_cnt < from_cnt {
                std::mem::swap(&mut from_cnt, &mut to_cnt);
            }

            let Some(result) = changes.get_by_atom_index(from_cnt) else { continue };
            for i in result.merged_index..changes.vec().len() {
                let change = &changes.vec()[i];
                if change.id.counter >= to_cnt {
                    break;
                }

                f(change)
            }
        }
    }

    /// iterates over all changes between LCA(common ancestors) to the merged version of (`from` and `to`) causally
    ///
    /// Tht iterator will include a version vector when the change is applied
    ///
    /// returns: (common_ancestor_vv, iterator)
    ///
    /// If frontiers are provided, it will be faster (because we don't need to calculate it from version vector
    pub(crate) fn iter_from_lca_causally(
        &self,
        from: &VersionVector,
        from_frontiers: Option<&Frontiers>,
        to: &VersionVector,
        to_frontiers: Option<&Frontiers>,
    ) -> (
        VersionVector,
        impl Iterator<Item = (&Change, Rc<RefCell<VersionVector>>)>,
    ) {
        debug_log::group!("iter_from_lca_causally");
        let mut merged_vv = from.clone();
        merged_vv.merge(to);
        let from_frontiers_inner;
        let to_frontiers_inner;

        let from_frontiers = match from_frontiers {
            Some(f) => f,
            None => {
                from_frontiers_inner = Some(from.to_frontiers(&self.dag));
                from_frontiers_inner.as_ref().unwrap()
            }
        };

        let to_frontiers = match to_frontiers {
            Some(t) => t,
            None => {
                to_frontiers_inner = Some(to.to_frontiers(&self.dag));
                to_frontiers_inner.as_ref().unwrap()
            }
        };

        let common_ancestors = self.dag.find_common_ancestor(from_frontiers, to_frontiers);
        let common_ancestors_vv = self.dag.frontiers_to_vv(&common_ancestors).unwrap();
        // go from lca to merged_vv
        let diff = common_ancestors_vv.diff(&merged_vv).right;
        let mut iter = self.dag.iter_causal(&common_ancestors, diff);
        let mut node = iter.next();
        let mut cur_cnt = 0;
        let vv = Rc::new(RefCell::new(VersionVector::default()));
        (
            common_ancestors_vv.clone(),
            std::iter::from_fn(move || {
                if let Some(inner) = &node {
                    let mut inner_vv = vv.borrow_mut();
                    inner_vv.clear();
                    inner_vv.extend_to_include_vv(inner.data.vv.iter());
                    let peer = inner.data.peer;
                    let cnt = inner
                        .data
                        .cnt
                        .max(cur_cnt)
                        .max(common_ancestors_vv.get(&peer).copied().unwrap_or(0));
                    let end = (inner.data.cnt + inner.data.len as Counter)
                        .min(merged_vv.get(&peer).copied().unwrap_or(0));
                    let change = self
                        .changes
                        .get(&peer)
                        .and_then(|x| x.get_by_atom_index(cnt).map(|x| x.element))
                        .unwrap();

                    if change.ctr_end() < end {
                        cur_cnt = change.ctr_end();
                    } else {
                        node = iter.next();
                        cur_cnt = 0;
                    }

                    inner_vv.extend_to_include_end_id(change.id);
                    // debug_log::debug_dbg!(&change, &inner_vv);
                    Some((change, vv.clone()))
                } else {
                    debug_log::group_end!();
                    None
                }
            }),
        )
    }

    pub(crate) fn len_changes(&self) -> usize {
        self.changes.values().map(|x| x.len()).sum()
    }

    pub fn diagnose_size(&self) {
        let mut total_changes = 0;
        let mut total_ops = 0;
        let mut total_atom_ops = 0;
        let total_dag_node = self.dag.map.len();
        for changes in self.changes.values() {
            total_changes += changes.len();
            for change in changes.iter() {
                total_ops += change.ops.len();
                total_atom_ops += change.atom_len();
            }
        }

        println!("total changes: {}", total_changes);
        println!("total ops: {}", total_ops);
        println!("total atom ops: {}", total_atom_ops);
        println!("total dag node: {}", total_dag_node);
    }
}

impl Default for OpLog {
    fn default() -> Self {
        Self::new()
    }
}
