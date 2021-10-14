use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{ErrorCode, Result, RwError};
use crate::expr::{build_from_proto as build_expr_from_proto, AggKind};
use crate::protobuf::Message as _;
use crate::storage::MemRowTable;
use crate::stream_op::*;
use crate::types::build_from_proto as build_type_from_proto;
use futures::channel::mpsc::{channel, Receiver, Sender};
use itertools::Itertools;
use risingwave_proto::expr::AggCall as AggCallProto;
use risingwave_proto::stream_plan;
use risingwave_proto::stream_service;
use std::convert::TryFrom;
use tokio::task::JoinHandle;

/// Default capacity of channel if two fragments are on the same node
pub const LOCAL_OUTPUT_CHANNEL_SIZE: usize = 16;

type ConsumableChannelPair = (Option<Sender<Message>>, Option<Receiver<Message>>);
type ConsumableChannelVecPair = (Vec<Sender<Message>>, Vec<Receiver<Message>>);

pub struct StreamManagerCore {
    /// Each processor runs in a future. Upon receiving a `Terminate` message, they will exit.
    /// `handles` store join handles of these futures, and therefore we could wait their
    /// termination.
    handles: Vec<JoinHandle<Result<()>>>,

    /// `channel_pool` store the senders and receivers for later `Processor`'s use. When `StreamManager`
    ///
    /// Each actor has several senders and several receivers. Senders and receivers are created during
    /// `update_fragment`. Upon `build_fragment`, all these channels will be taken out and built into
    /// the operators and outputs.
    channel_pool: HashMap<u32, ConsumableChannelVecPair>,

    /// Stores all actor information
    actors: HashMap<u32, stream_service::ActorInfo>,

    /// Stores all fragment information
    fragments: HashMap<u32, stream_plan::StreamFragment>,

    /// Mock source, `fragment_id = 0`
    /// TODO: remove this
    mock_source: ConsumableChannelPair,
}

/// `StreamManager` manages all stream operators in this project.
pub struct StreamManager {
    core: Mutex<StreamManagerCore>,
}

impl StreamManager {
    pub fn new() -> Self {
        StreamManager {
            core: Mutex::new(StreamManagerCore::new()),
        }
    }

    pub fn update_fragment(&self, fragments: &[stream_plan::StreamFragment]) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.update_fragment(fragments)
    }

    pub async fn wait_all(&self) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.wait_all().await
    }

    /// This function could only be called once during the lifecycle of `StreamManager` for now.
    pub fn update_actor_info(&self, table: stream_service::ActorInfoTable) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.update_actor_info(table)
    }

    /// This function could only be called once during the lifecycle of `StreamManager` for now.
    pub fn build_fragment(&self, fragments: &[u32]) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        core.build_fragment(fragments)
    }

    #[cfg(test)]
    pub fn take_source(&self) -> Sender<Message> {
        let mut core = self.core.lock().unwrap();
        core.mock_source.0.take().unwrap()
    }

    #[cfg(test)]
    pub fn take_sink(&self) -> Receiver<Message> {
        let mut core = self.core.lock().unwrap();
        core.channel_pool.get_mut(&233).unwrap().1.remove(0)
    }
}

fn build_agg_call_from_proto(agg_call_proto: &AggCallProto) -> Result<AggCall> {
    let args = {
        let args = agg_call_proto.get_args();
        match args {
            [] => AggArgs::None,
            [arg] => AggArgs::Unary(
                build_type_from_proto(arg.get_field_type())?,
                arg.get_input().column_idx as usize,
            ),
            _ => {
                return Err(RwError::from(ErrorCode::NotImplementedError(
                    "multiple aggregation args".to_string(),
                )))
            }
        }
    };
    Ok(AggCall {
        kind: AggKind::try_from(agg_call_proto.get_field_type())?,
        args,
        return_type: build_type_from_proto(agg_call_proto.get_return_type())?,
    })
}

impl StreamManagerCore {
    fn new() -> Self {
        let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
        Self {
            handles: vec![],
            channel_pool: HashMap::new(),
            actors: HashMap::new(),
            fragments: HashMap::new(),
            mock_source: (Some(tx), Some(rx)),
        }
    }

    /// Create dispatchers with downstream information registered before
    fn create_dispatcher(
        &mut self,
        input: Box<dyn StreamOperator>,
        dispatcher: &stream_plan::Dispatcher,
        fragment_id: u32,
        downstreams: &[u32],
    ) -> Box<dyn StreamConsumer> {
        // create downstream receivers
        let outputs = self
            .channel_pool
            .get_mut(&fragment_id)
            .map(|x| std::mem::take(&mut x.0))
            .unwrap_or_default()
            .into_iter()
            .map(|tx| Box::new(ChannelOutput::new(tx)) as Box<dyn Output>)
            .collect::<Vec<_>>();

        assert_eq!(downstreams.len(), outputs.len());

        use stream_plan::Dispatcher_DispatcherType::*;
        match dispatcher.field_type {
            ROUND_ROBIN => {
                assert!(!outputs.is_empty());
                Box::new(DispatchExecutor::new(
                    input,
                    RoundRobinDataDispatcher::new(outputs),
                ))
            }
            HASH => {
                assert!(!outputs.is_empty());
                Box::new(DispatchExecutor::new(
                    input,
                    HashDataDispatcher::new(outputs, vec![dispatcher.get_column_idx() as usize]),
                ))
            }
            BROADCAST => {
                assert!(!outputs.is_empty());
                Box::new(DispatchExecutor::new(
                    input,
                    BroadcastDispatcher::new(outputs),
                ))
            }
            SIMPLE => {
                assert_eq!(outputs.len(), 1);
                let output = outputs.into_iter().next().unwrap();
                Box::new(DispatchExecutor::new(input, SimpleDispatcher::new(output)))
            }
            BLACKHOLE => Box::new(DispatchExecutor::new(input, BlackHoleDispatcher::new())),
        }
    }

    /// Create a chain of nodes and return the head operator
    fn create_nodes(
        &mut self,
        node: &stream_plan::StreamNode,
        merger: Box<dyn StreamOperator>,
    ) -> Result<Box<dyn StreamOperator>> {
        use stream_plan::StreamNode_StreamNodeType::*;

        // Create the input operator before creating itself
        // Note(eric): Maybe we should put a `Merger` operator in proto
        let input = if node.has_input() {
            self.create_nodes(node.get_input(), merger)?
        } else {
            merger
        };

        let operator: Box<dyn StreamOperator> = match node.get_node_type() {
            PROJECTION => {
                let project_node =
                    stream_plan::ProjectNode::parse_from_bytes(node.get_body().get_value())
                        .map_err(ErrorCode::ProtobufError)?;
                let project_exprs = project_node
                    .get_select_list()
                    .iter()
                    .map(build_expr_from_proto)
                    .collect::<Result<Vec<_>>>()?;
                Box::new(ProjectionOperator::new(input, project_exprs))
            }
            FILTER => {
                let filter_node =
                    stream_plan::FilterNode::parse_from_bytes(node.get_body().get_value())
                        .map_err(ErrorCode::ProtobufError)?;
                let search_condition = build_expr_from_proto(filter_node.get_search_condition())?;
                Box::new(FilterOperator::new(input, search_condition))
            }
            LOCAL_SIMPLE_AGG => {
                let aggr_node =
                    stream_plan::SimpleAggNode::parse_from_bytes(node.get_body().get_value())
                        .map_err(ErrorCode::ProtobufError)?;
                let agg_calls: Vec<AggCall> = aggr_node
                    .get_agg_calls()
                    .iter()
                    .map(build_agg_call_from_proto)
                    .try_collect()?;
                Box::new(SimpleAggExecutor::new(input, agg_calls)?)
            }
            GLOBAL_SIMPLE_AGG => todo!(),
            // TODO: There will be only one hash aggregation, combining LOCAL and GLOBAL
            LOCAL_HASH_AGG => {
                let aggr_node =
                    stream_plan::HashAggNode::parse_from_bytes(node.get_body().get_value())
                        .map_err(ErrorCode::ProtobufError)?;

                let keys = aggr_node
                    .get_group_keys()
                    .iter()
                    .map(|key| key.column_idx as usize)
                    .collect::<Vec<_>>();

                let agg_calls: Vec<AggCall> = aggr_node
                    .get_agg_calls()
                    .iter()
                    .map(build_agg_call_from_proto)
                    .try_collect()?;

                Box::new(HashAggregationOperator::new(input, agg_calls, keys))
            }
            GLOBAL_HASH_AGG => todo!(),
            MEMTABLE_MATERIALIZED_VIEW => {
                let mtmv = stream_plan::MemTableMaterializedViewNode::parse_from_bytes(
                    node.get_body().get_value(),
                )
                .map_err(ErrorCode::ProtobufError)?;

                // TODO: assign a memtable from manager
                let memtable = MemRowTable::default();

                Box::new(MemTableMVOperator::new(
                    input,
                    std::sync::Arc::new(memtable),
                    mtmv.get_pk_idx().iter().map(|x| *x as usize).collect(),
                ))
            }
            others => todo!("unsupported StreamNodeType: {:?}", others),
        };
        Ok(operator)
    }

    fn create_merger(
        &mut self,
        fragment_id: u32,
        upstreams: &[u32],
    ) -> Result<Box<dyn StreamOperator>> {
        assert!(!upstreams.is_empty());

        let mut rxs = self
            .channel_pool
            .get_mut(&fragment_id)
            .map(|x| std::mem::take(&mut x.1))
            .unwrap_or_default();

        for upstream in upstreams {
            // TODO: remove this
            if *upstream == 0 {
                // `fragment_id = 0` is used as mock input
                rxs.push(self.mock_source.1.take().unwrap());
                continue;
            }

            let actor = self
                .actors
                .get(upstream)
                .expect("upstream actor not found in info table");
            // FIXME: use `is_local_address` from `ExchangeExecutor`.
            if actor.get_host().get_host() == "127.0.0.1" {
                continue;
            } else {
                todo!("remote node is not supported in streaming engine");
                // TODO: create gRPC connection
            }
        }

        assert_eq!(
            rxs.len(),
            upstreams.len(),
            "upstreams are not fully available: {} registered while {} required, fragment_id={}",
            rxs.len(),
            upstreams.len(),
            fragment_id
        );

        if upstreams.len() == 1 {
            // Only one upstream, use `ReceiverOperator`.
            Ok(Box::new(ReceiverOperator::new(rxs.remove(0))))
        } else {
            Ok(Box::new(MergeOperator::new(rxs)))
        }
    }

    fn build_fragment(&mut self, fragments: &[u32]) -> Result<()> {
        for fragment_id in fragments {
            let fragment = self.fragments.remove(fragment_id).unwrap();

            let merger = self.create_merger(*fragment_id, fragment.get_upstream_fragment_id())?;

            let operator = self.create_nodes(fragment.get_nodes(), merger)?;

            let dispatcher = self.create_dispatcher(
                operator,
                fragment.get_dispatcher(),
                *fragment_id,
                fragment.get_downstream_fragment_id(),
            );

            let actor = Actor::new(dispatcher);
            tokio::spawn(actor.run());
        }

        Ok(())
    }

    pub async fn wait_all(&mut self) -> Result<()> {
        for handle in std::mem::take(&mut self.handles) {
            handle.await.unwrap()?;
        }
        Ok(())
    }

    fn update_actor_info(&mut self, table: stream_service::ActorInfoTable) -> Result<()> {
        for actor in table.get_info() {
            let ret = self.actors.insert(actor.get_fragment_id(), actor.clone());
            if ret.is_some() {
                return Err(ErrorCode::InternalError(format!(
                    "duplicated actor {}",
                    actor.get_fragment_id()
                ))
                .into());
            }
        }
        Ok(())
    }

    fn update_fragment(&mut self, fragments: &[stream_plan::StreamFragment]) -> Result<()> {
        for fragment in fragments {
            let ret = self
                .fragments
                .insert(fragment.get_fragment_id(), fragment.clone());
            if ret.is_some() {
                return Err(ErrorCode::InternalError(format!(
                    "duplicated fragment {}",
                    fragment.get_fragment_id()
                ))
                .into());
            }
        }

        for (current_id, fragment) in &self.fragments {
            for downstream in fragment.get_downstream_fragment_id() {
                // At this time, the graph might not be complete, so we do not check if downstream has `current_id`
                // as upstream.
                let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
                let current_channels = self.channel_pool.entry(*current_id).or_default();
                current_channels.0.push(tx);
                let downstream_channels = self.channel_pool.entry(*downstream).or_default();
                downstream_channels.1.push(rx);
            }
        }
        Ok(())
    }
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}
