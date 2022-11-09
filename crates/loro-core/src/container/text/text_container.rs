use std::ops::Range;

use rle::{
    rle_tree::{tree_trait::CumulateTreeTrait, HeapMode},
    HasLength, RleTree, Sliceable,
};
use smallvec::{smallvec, SmallVec};
use tabled::object::LastColumn;

use crate::{
    container::{
        list::list_op::{DeleteSpan, ListOp},
        Container, ContainerID, ContainerType,
    },
    dag::DagUtils,
    debug_log,
    id::{Counter, ID},
    log_store::LogStoreWeakRef,
    op::{InsertContent, Op, OpContent, RemoteOp},
    smstring::SmString,
    span::{HasCounterSpan, HasIdSpan, IdSpan},
    value::LoroValue,
    LogStore, VersionVector,
};

use super::{
    string_pool::StringPool,
    text_content::ListSlice,
    tracker::{Effect, Tracker},
};

#[derive(Clone, Debug)]
struct DagNode {
    id: IdSpan,
    deps: SmallVec<[ID; 2]>,
}

#[derive(Debug)]
pub struct TextContainer {
    id: ContainerID,
    log_store: LogStoreWeakRef,
    state: RleTree<Range<u32>, CumulateTreeTrait<Range<u32>, 8, HeapMode>>,
    raw_str: StringPool,
    tracker: Tracker,

    head: SmallVec<[ID; 2]>,
    vv: VersionVector,
}

impl TextContainer {
    pub(crate) fn new(id: ContainerID, log_store: LogStoreWeakRef) -> Self {
        Self {
            id,
            log_store,
            raw_str: StringPool::default(),
            tracker: Tracker::new(Default::default(), 0),
            state: Default::default(),
            // TODO: should be eq to log_store frontier?
            head: Default::default(),
            vv: Default::default(),
        }
    }

    pub fn insert(&mut self, pos: usize, text: &str) -> Option<ID> {
        if text.is_empty() {
            return None;
        }

        let s = self.log_store.upgrade().unwrap();
        let mut store = s.write().unwrap();
        let id = store.next_id();
        let slice = self.raw_str.alloc(text);
        self.state.insert(pos, slice.clone());
        let op = Op::new(
            id,
            OpContent::Normal {
                content: InsertContent::List(ListOp::Insert {
                    slice: ListSlice::Slice(slice),
                    pos,
                }),
            },
            store.get_or_create_container_idx(&self.id),
        );
        let last_id = ID::new(
            store.this_client_id,
            op.counter + op.atom_len() as Counter - 1,
        );
        store.append_local_ops(&[op]);
        self.head = smallvec![last_id];
        self.vv.set_last(last_id);

        Some(id)
    }

    pub fn delete(&mut self, pos: usize, len: usize) -> Option<ID> {
        if len == 0 {
            return None;
        }

        let s = self.log_store.upgrade().unwrap();
        let mut store = s.write().unwrap();
        let id = store.next_id();
        let op = Op::new(
            id,
            OpContent::Normal {
                content: InsertContent::List(ListOp::new_del(pos, len)),
            },
            store.get_or_create_container_idx(&self.id),
        );

        let last_id = ID::new(store.this_client_id, op.ctr_last());
        store.append_local_ops(&[op]);
        self.state.delete_range(Some(pos), Some(pos + len));
        self.head = smallvec![last_id];
        self.vv.set_last(last_id);
        Some(id)
    }

    pub fn text_len(&self) -> usize {
        self.state.len()
    }

    pub fn check(&mut self) {
        self.tracker.check();
    }

    #[cfg(feature = "fuzzing")]
    pub fn debug_inspect(&mut self) {
        println!(
            "Text Container {:?}, Raw String size={}, Tree=>\n",
            self.id,
            self.raw_str.len(),
        );
        self.state.debug_inspect();
    }
}

impl Container for TextContainer {
    fn id(&self) -> &ContainerID {
        &self.id
    }

    fn type_(&self) -> ContainerType {
        ContainerType::Text
    }

    // TODO: move main logic to tracker module
    fn apply(&mut self, id_span: IdSpan, store: &LogStore) {
        debug_log!("APPLY ENTRY client={}", store.this_client_id);
        let self_idx = store.get_container_idx(&self.id).unwrap();
        let new_op_id = id_span.id_last();
        // TODO: may reduce following two into one op
        let common_ancestors = store.find_common_ancestor(&[new_op_id], &self.head);
        if common_ancestors == self.head {
            let latest_head = smallvec![new_op_id];
            let path = store.find_path(&self.head, &latest_head);
            if path.right.len() == 1 {
                // linear updates, we can apply them directly
                let start = self.vv.get(&new_op_id.client_id).copied().unwrap_or(0);
                for op in store.iter_ops_at_id_span(
                    IdSpan::new(new_op_id.client_id, start, new_op_id.counter + 1),
                    self.id.clone(),
                ) {
                    let op = op.get_sliced();
                    match &op.content {
                        OpContent::Normal {
                            content: InsertContent::List(op),
                        } => match op {
                            ListOp::Insert { slice, pos } => {
                                self.state.insert(*pos, slice.as_slice().unwrap().clone())
                            }
                            ListOp::Delete(span) => self.state.delete_range(
                                Some(span.start() as usize),
                                Some(span.end() as usize),
                            ),
                        },
                        _ => unreachable!(),
                    }
                }

                self.head = latest_head;
                self.vv.set_last(new_op_id);
                return;
            } else {
                let path: Vec<_> = store.iter_partial(&self.head, path.right).collect();
                if path
                    .iter()
                    .all(|x| x.forward.is_empty() && x.retreat.is_empty())
                {
                    // if we don't need to retreat or forward, we can update the state directly
                    for iter in path {
                        let change = iter
                            .data
                            .slice(iter.slice.start as usize, iter.slice.end as usize);
                        for op in change.ops.iter() {
                            if op.container == self_idx {
                                match &op.content {
                                    OpContent::Normal {
                                        content: InsertContent::List(op),
                                    } => match op {
                                        ListOp::Insert { slice, pos } => self
                                            .state
                                            .insert(*pos, slice.as_slice().unwrap().clone()),
                                        ListOp::Delete(span) => self.state.delete_range(
                                            Some(span.start() as usize),
                                            Some(span.end() as usize),
                                        ),
                                    },
                                    _ => unreachable!(),
                                }
                            }
                        }
                    }

                    self.head = latest_head;
                    self.vv.set_last(new_op_id);
                    return;
                }
            }
        }

        let path_to_head = store.find_path(&common_ancestors, &self.head);
        let mut common_ancestors_vv = self.vv.clone();
        common_ancestors_vv.retreat(&path_to_head.right);
        let mut latest_head: SmallVec<[ID; 2]> = self.head.clone();
        latest_head.retain(|x| !common_ancestors_vv.includes_id(*x));
        latest_head.push(new_op_id);
        // println!("{}", store.mermaid());
        debug_log!(
            "START FROM HEADS={:?} new_op_id={} self.head={:?}",
            &common_ancestors,
            new_op_id,
            &self.head
        );

        let head = if (common_ancestors.is_empty() && !self.tracker.start_vv().is_empty())
            || !common_ancestors.iter().all(|x| self.tracker.contains(*x))
        {
            debug_log!("NewTracker");
            self.tracker = Tracker::new(common_ancestors_vv, Counter::MAX / 2);
            common_ancestors
        } else {
            debug_log!("OldTracker");
            self.tracker.checkout_to_latest();
            self.tracker.all_vv().get_head()
        };

        // stage 1
        let path = store.find_path(&head, &latest_head);
        debug_log!("path={:?}", &path.right);
        for iter in store.iter_partial(&head, path.right) {
            // TODO: avoid this clone
            let change = iter
                .data
                .slice(iter.slice.start as usize, iter.slice.end as usize);
            debug_log!(
                "Stage1 retreat:{} forward:{}\n{}",
                format!("{:?}", &iter.retreat).red(),
                format!("{:?}", &iter.forward).red(),
                format!("{:#?}", &change).blue(),
            );
            self.tracker.retreat(&iter.retreat);
            self.tracker.forward(&iter.forward);
            for op in change.ops.iter() {
                if op.container == self_idx {
                    // TODO: convert op to local
                    self.tracker.apply(
                        ID {
                            client_id: change.id.client_id,
                            counter: op.counter,
                        },
                        &op.content,
                    )
                }
            }
        }

        // stage 2
        // TODO: reduce computations
        let path = store.find_path(&self.head, &latest_head);
        debug_log!("BEFORE CHECKOUT");
        // dbg!(&self.tracker);
        self.tracker.checkout(self.vv.clone());
        debug_log!("AFTER CHECKOUT");
        // dbg!(&self.tracker);
        debug_log!(
            "[Stage 2]: Iterate path: {} from {} => {}",
            format!("{:?}", path.right).red(),
            format!("{:?}", self.head).red(),
            format!("{:?}", latest_head).red(),
        );
        debug_log!(
            "BEFORE EFFECT STATE={}",
            self.get_value().as_string().unwrap()
        );
        for effect in self.tracker.iter_effects(path.right) {
            debug_log!("EFFECT: {:?}", &effect);
            match effect {
                Effect::Del { pos, len } => self.state.delete_range(Some(pos), Some(pos + len)),
                Effect::Ins { pos, content } => {
                    self.state.insert(pos, content.as_slice().unwrap().clone());
                }
            }
            debug_log!("AFTER EFFECT");
        }
        debug_log!(
            "AFTER EFFECT STATE={}",
            self.get_value().as_string().unwrap()
        );

        self.head = latest_head;
        self.vv.set_last(new_op_id);
        debug_log!("--------------------------------");
    }

    fn checkout_version(&mut self, _vv: &crate::VersionVector) {
        todo!()
    }

    // TODO: maybe we need to let this return Cow
    fn get_value(&self) -> LoroValue {
        let mut ans_str = String::new();
        for v in self.state.iter() {
            let content = v.as_ref();
            ans_str.push_str(&self.raw_str.get_str(content));
        }

        LoroValue::String(ans_str.into_boxed_str())
    }

    fn to_export(&self, op: &mut Op) {
        if let Some((slice, _pos)) = op
            .content
            .as_normal_mut()
            .and_then(|c| c.as_list_mut())
            .and_then(|x| x.as_insert_mut())
        {
            if let Some(change) = if let ListSlice::Slice(ranges) = slice {
                Some(self.raw_str.get_str(ranges))
            } else {
                None
            } {
                *slice = ListSlice::RawStr(change);
            }
        }
    }

    fn to_import(&mut self, op: &mut RemoteOp) {
        if let Some((slice, _pos)) = op
            .content
            .as_normal_mut()
            .and_then(|c| c.as_list_mut())
            .and_then(|x| x.as_insert_mut())
        {
            if let Some(slice_range) = match slice {
                ListSlice::RawStr(s) => {
                    let range = self.raw_str.alloc(s);
                    Some(range)
                }
                ListSlice::Slice(_) => unreachable!(),
                ListSlice::Unknown(_) => unreachable!(),
            } {
                *slice = ListSlice::Slice(slice_range);
            }
        }
    }
}
