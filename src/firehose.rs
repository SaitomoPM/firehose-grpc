use crate::datasource::{
    Block, BlockHeader, CallType, DataRequest, DataSource, HashAndHeight, HotDataSource, Log,
    LogRequest, Trace, TraceType, Transaction,
};
use crate::pbcodec;
use crate::pbfirehose::single_block_request::Reference;
use crate::pbfirehose::{ForkStep, Request, Response, SingleBlockRequest, SingleBlockResponse};
use crate::pbtransforms::CombinedFilter;
use anyhow::Context;
use async_stream::try_stream;
use futures_core::stream::Stream;
use futures_util::stream::StreamExt;
use prost::Message;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

async fn resolve_negative_start(
    start_block_num: i64,
    archive: &(dyn DataSource + Send + Sync),
) -> anyhow::Result<u64> {
    if start_block_num < 0 {
        let delta = u64::try_from(start_block_num.abs())?;
        let head = archive.get_finalized_height().await?;
        return Ok(head.saturating_sub(delta));
    }
    Ok(u64::try_from(start_block_num)?)
}

fn vec_from_hex(value: &str) -> Result<Vec<u8>, prefix_hex::Error> {
    let buf: Vec<u8> = if value.len() % 2 != 0 {
        let value = format!("0x0{}", &value[2..]);
        prefix_hex::decode(value)?
    } else {
        prefix_hex::decode(value)?
    };

    Ok(buf)
}

fn qty2int(value: &String) -> anyhow::Result<u64> {
    Ok(u64::from_str_radix(value.trim_start_matches("0x"), 16)?)
}

pub struct Firehose {
    archive: Arc<dyn DataSource + Sync + Send>,
    rpc: Arc<dyn HotDataSource + Sync + Send>,
}

impl Firehose {
    pub fn new(
        archive: Arc<dyn DataSource + Sync + Send>,
        rpc: Arc<dyn HotDataSource + Sync + Send>,
    ) -> Firehose {
        Firehose { archive, rpc }
    }

    pub async fn blocks(
        &self,
        request: Request,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<Response>>> {
        let from_block = resolve_negative_start(request.start_block_num, self.rpc.as_ds()).await?;
        let to_block = if request.stop_block_num == 0 {
            None
        } else {
            Some(request.stop_block_num)
        };

        let mut logs: Vec<LogRequest> = vec![];
        for transform in &request.transforms {
            let filter = CombinedFilter::decode(&transform.value[..])?;
            for log_filter in filter.log_filters {
                let log_request = LogRequest {
                    address: log_filter
                        .addresses
                        .into_iter()
                        .map(|address| prefix_hex::encode(address))
                        .collect(),
                    topic0: log_filter
                        .event_signatures
                        .into_iter()
                        .map(|signature| prefix_hex::encode(signature))
                        .collect(),
                };
                logs.push(log_request);
            }
        }

        let archive = self.archive.clone();
        let rpc = self.rpc.clone();

        Ok(try_stream! {
            let mut state = None;
            let mut from_block = from_block;

            let archive_height = archive.get_finalized_height().await?;
            if from_block < archive_height {
                let req = DataRequest {
                    from: from_block,
                    to: to_block,
                    logs: logs.clone(),
                    transactions: vec![],
                };
                let mut stream = Pin::from(archive.get_finalized_blocks(req)?);
                while let Some(result) = stream.next().await {
                    let blocks = result?;
                    for block in blocks {
                        state = Some(HashAndHeight {
                            hash: block.header.hash.clone(),
                            height: block.header.number,
                        });
                        from_block = block.header.number + 1;

                        let graph_block = pbcodec::Block::try_from(block)?;

                        yield Response {
                            block: Some(prost_types::Any {
                                type_url: "type.googleapis.com/sf.ethereum.type.v2.Block".to_string(),
                                value: graph_block.encode_to_vec(),
                            }),
                            step: ForkStep::StepNew.into(),
                            cursor: graph_block.number.to_string(),
                        };
                    }
                }

                if let Some(to_block) = to_block {
                    if state.as_ref().unwrap().height == to_block {
                        return
                    }
                }
            }

            let rpc_height = rpc.get_finalized_height().await?;
            if from_block < rpc_height {
                let to = if let Some(to_block) = to_block {
                    std::cmp::min(to_block, rpc_height)
                } else {
                    rpc_height
                };
                let req = DataRequest {
                    from: from_block,
                    to: Some(to),
                    logs: logs.clone(),
                    transactions: vec![],
                };
                let mut stream = Pin::from(rpc.get_finalized_blocks(req)?);
                while let Some(result) = stream.next().await {
                    let blocks = result?;
                    for block in blocks {
                        let graph_block = pbcodec::Block::try_from(block)?;

                        yield Response {
                            block: Some(prost_types::Any {
                                type_url: "type.googleapis.com/sf.ethereum.type.v2.Block".to_string(),
                                value: graph_block.encode_to_vec(),
                            }),
                            step: ForkStep::StepNew.into(),
                            cursor: graph_block.number.to_string(),
                        };
                    }
                }
                state = Some(HashAndHeight {
                    hash: rpc.get_block_hash(to).await?,
                    height: to,
                });
                from_block = to + 1;

                if let Some(to_block) = to_block {
                    if state.as_ref().unwrap().height == to_block {
                        return
                    }
                }
            }

            let req = DataRequest {
                from: from_block,
                to: to_block,
                logs,
                transactions: vec![],
            };
            let state = state.context("state isn't expected to be None")?;
            let mut last_head = state.clone();
            let mut stream = Pin::from(rpc.get_hot_blocks(req, state)?);
            while let Some(result) = stream.next().await {
                let upd = result?;

                let new_head = if upd.blocks.is_empty() {
                    upd.base_head.clone()
                } else {
                    let header = &upd.blocks.last().unwrap().header;
                    HashAndHeight {
                        hash: header.hash.clone(),
                        height: header.number,
                    }
                };

                if upd.base_head != last_head {
                    // fork happened
                    // only number and parent_hash are required for ForkStep::StepUndo
                    let mut graph_block = pbcodec::Block::default();
                    let mut header = pbcodec::BlockHeader::default();
                    header.number = last_head.height;
                    header.parent_hash = prefix_hex::decode(upd.base_head.hash)?;
                    graph_block.header = Some(header);

                    yield Response {
                        block: Some(prost_types::Any {
                            type_url: "type.googleapis.com/sf.ethereum.type.v2.Block".to_string(),
                            value: graph_block.encode_to_vec(),
                        }),
                        step: ForkStep::StepUndo.into(),
                        cursor: last_head.height.to_string(),
                    };
                }

                for block in upd.blocks {
                    let graph_block = pbcodec::Block::try_from(block)?;
                    yield Response {
                        block: Some(prost_types::Any {
                            type_url: "type.googleapis.com/sf.ethereum.type.v2.Block".to_string(),
                            value: graph_block.encode_to_vec(),
                        }),
                        step: ForkStep::StepNew.into(),
                        cursor: graph_block.number.to_string(),
                    }
                }

                last_head = new_head;
            }
        })
    }

    pub async fn block(&self, request: SingleBlockRequest) -> anyhow::Result<SingleBlockResponse> {
        let block_num = match request.reference.as_ref().unwrap() {
            Reference::BlockNumber(block_number) => block_number.num,
            Reference::BlockHashAndNumber(block_hash_and_number) => block_hash_and_number.num,
            Reference::Cursor(cursor) => cursor.cursor.parse().unwrap(),
        };

        let req = DataRequest {
            from: block_num,
            to: Some(block_num),
            logs: vec![],
            transactions: vec![],
        };

        let mut stream = Pin::from(self.archive.get_finalized_blocks(req)?);
        let blocks = stream.next().await.unwrap()?;
        let block = blocks.into_iter().nth(0).unwrap();

        let graph_block = pbcodec::Block::try_from(block)?;

        Ok(SingleBlockResponse {
            block: Some(prost_types::Any {
                type_url: "type.googleapis.com/sf.ethereum.type.v2.Block".to_string(),
                value: graph_block.encode_to_vec(),
            }),
        })
    }
}

impl TryFrom<BlockHeader> for pbcodec::BlockHeader {
    type Error = anyhow::Error;

    fn try_from(value: BlockHeader) -> anyhow::Result<Self, Self::Error> {
        Ok(pbcodec::BlockHeader {
            parent_hash: prefix_hex::decode(value.parent_hash)?,
            uncle_hash: prefix_hex::decode(value.sha3_uncles)?,
            coinbase: prefix_hex::decode(value.miner)?,
            state_root: prefix_hex::decode(value.state_root)?,
            transactions_root: prefix_hex::decode(value.transactions_root)?,
            receipt_root: prefix_hex::decode(value.receipts_root)?,
            logs_bloom: prefix_hex::decode(value.logs_bloom)?,
            difficulty: Some(pbcodec::BigInt {
                bytes: vec_from_hex(&value.difficulty)?,
            }),
            total_difficulty: Some(pbcodec::BigInt {
                bytes: vec_from_hex(&value.total_difficulty)?,
            }),
            number: value.number,
            gas_limit: qty2int(&value.gas_limit)?,
            gas_used: qty2int(&value.gas_used)?,
            timestamp: Some(prost_types::Timestamp {
                seconds: i64::try_from(value.timestamp)?,
                nanos: 0,
            }),
            extra_data: prefix_hex::decode(value.extra_data)?,
            mix_hash: prefix_hex::decode(value.mix_hash)?,
            nonce: qty2int(&value.nonce)?,
            hash: prefix_hex::decode(value.hash)?,
            base_fee_per_gas: value.base_fee_per_gas.map_or::<anyhow::Result<_>, _>(
                Ok(None),
                |val| {
                    Ok(Some(pbcodec::BigInt {
                        bytes: vec_from_hex(&val)?,
                    }))
                },
            )?,
        })
    }
}

impl TryFrom<Transaction> for pbcodec::TransactionTrace {
    type Error = anyhow::Error;

    fn try_from(value: Transaction) -> Result<Self, Self::Error> {
        Ok(pbcodec::TransactionTrace {
            to: prefix_hex::decode(
                value
                    .to
                    .unwrap_or("0x0000000000000000000000000000000000000000".to_string()),
            )?,
            nonce: value.nonce,
            gas_price: Some(pbcodec::BigInt {
                bytes: vec_from_hex(&value.gas_price)?,
            }),
            gas_limit: qty2int(&value.gas)?,
            gas_used: qty2int(&value.gas_used)?,
            value: Some(pbcodec::BigInt {
                bytes: vec_from_hex(&value.value)?,
            }),
            input: prefix_hex::decode(value.input)?,
            v: vec_from_hex(&value.v)?,
            r: vec_from_hex(&value.r)?,
            s: vec_from_hex(&value.s)?,
            r#type: value.r#type,
            access_list: vec![],
            max_fee_per_gas: value.max_fee_per_gas.map_or::<anyhow::Result<_>, _>(
                Ok(None),
                |val| {
                    Ok(Some(pbcodec::BigInt {
                        bytes: vec_from_hex(&val)?,
                    }))
                },
            )?,
            max_priority_fee_per_gas: value
                .max_priority_fee_per_gas
                .map_or::<anyhow::Result<_>, _>(Ok(None), |val| {
                    Ok(Some(pbcodec::BigInt {
                        bytes: vec_from_hex(&val)?,
                    }))
                })?,
            index: value.transaction_index,
            hash: prefix_hex::decode(value.hash)?,
            from: prefix_hex::decode(value.from)?,
            return_data: vec![],
            public_key: vec![],
            begin_ordinal: 0,
            end_ordinal: 0,
            status: value.status,
            receipt: None,
            calls: vec![],
        })
    }
}

impl TryFrom<Block> for pbcodec::Block {
    type Error = anyhow::Error;

    fn try_from(value: Block) -> Result<Self, Self::Error> {
        let mut logs_by_tx: HashMap<u32, Vec<Log>> = HashMap::new();
        for log in value.logs {
            if logs_by_tx.contains_key(&log.transaction_index) {
                logs_by_tx
                    .get_mut(&log.transaction_index)
                    .unwrap()
                    .push(log);
            } else {
                logs_by_tx.insert(log.transaction_index, vec![log]);
            }
        }

        let mut traces_by_tx: HashMap<u32, Vec<Trace>> = HashMap::new();
        for trace in value.traces {
            if traces_by_tx.contains_key(&trace.transaction_index) {
                traces_by_tx
                    .get_mut(&trace.transaction_index)
                    .unwrap()
                    .push(trace);
            } else {
                traces_by_tx.insert(trace.transaction_index, vec![trace]);
            }
        }

        let transaction_traces = value.transactions.into_iter().map(|tx| {
            let logs = logs_by_tx.remove(&tx.transaction_index).unwrap_or_default().into_iter().map(|log| pbcodec::Log {
                address: prefix_hex::decode(log.address).unwrap(),
                data: prefix_hex::decode(log.data).unwrap(),
                block_index: log.log_index,
                topics: log.topics.into_iter().map(|topic| prefix_hex::decode(topic).unwrap()).collect(),
                index: log.transaction_index,
                ordinal: 0,
            }).collect();
            let calls = traces_by_tx.remove(&tx.transaction_index).unwrap_or_default().into_iter().filter_map(|trace| {
                let (action, result) = match trace.r#type {
                    TraceType::Create | TraceType::Call => (trace.action.unwrap(), trace.result),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let call_type = match trace.r#type {
                    TraceType::Create => 5,
                    TraceType::Call => match action.r#type.unwrap() {
                        CallType::Call => 1,
                        CallType::Callcode => 2,
                        CallType::Delegatecall => 3,
                        CallType::Staticcall => 4,
                    },
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let caller = match trace.r#type {
                    TraceType::Create => action.from.unwrap(),
                    TraceType::Call => action.from.unwrap(),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let address = match trace.r#type {
                    TraceType::Create => result.clone().unwrap().address.unwrap(),
                    TraceType::Call => action.to.unwrap(),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let value = match trace.r#type {
                    TraceType::Create => action.value.unwrap(),
                    TraceType::Call => action.value.unwrap(),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let gas = match trace.r#type {
                    TraceType::Create => action.gas.unwrap(),
                    TraceType::Call => action.gas.unwrap(),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let gas_used = match trace.r#type {
                    TraceType::Create => result.clone().unwrap().gas_used.unwrap(),
                    TraceType::Call => if result.is_some() {result.clone().unwrap().gas_used.unwrap()} else {"0x0".to_string()},
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let output = match trace.r#type {
                    TraceType::Create => "0x".to_string(),
                    TraceType::Call => if result.is_some() {result.clone().unwrap().output.unwrap()} else {"0x".to_string()},
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                let input = match trace.r#type {
                    TraceType::Create => "0x".to_string(),
                    TraceType::Call => action.input.unwrap(),
                    TraceType::Suicide | TraceType::Reward => return None,
                };
                Some(pbcodec::Call {
                    index: 0,
                    parent_index: 0,
                    depth: 0,
                    call_type,
                    caller: vec_from_hex(&caller).unwrap(),
                    address: vec_from_hex(&address).unwrap(),
                    value: Some(pbcodec::BigInt { bytes: vec_from_hex(&value).unwrap() }),
                    gas_limit: u64::from_str_radix(&gas.trim_start_matches("0x"), 16).unwrap(),
                    gas_consumed: u64::from_str_radix(&gas_used.trim_start_matches("0x"), 16).unwrap(),
                    return_data: vec_from_hex(&output).unwrap(),
                    input: vec_from_hex(&input).unwrap(),
                    executed_code: false,
                    suicide: false,
                    keccak_preimages: HashMap::new(),
                    storage_changes: vec![],
                    balance_changes: vec![],
                    nonce_changes: vec![],
                    logs: vec![],
                    code_changes: vec![],
                    gas_changes: vec![],
                    status_failed: trace.error.is_some() || trace.revert_reason.is_some(),
                    status_reverted: trace.revert_reason.is_some(),
                    failure_reason: trace.error.unwrap_or_else(|| trace.revert_reason.unwrap_or_default()),
                    state_reverted: false,
                    begin_ordinal: 0,
                    end_ordinal: 0,
                    account_creations: vec![],
                })
            }).collect();
            let receipt = pbcodec::TransactionReceipt {
                state_root: vec![],
                cumulative_gas_used: qty2int(&tx.cumulative_gas_used)?,
                logs_bloom: prefix_hex::decode("0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000")?,
                logs,
            };
            let mut tx_trace = pbcodec::TransactionTrace::try_from(tx)?;
            tx_trace.receipt = Some(receipt);
            tx_trace.calls = calls;
            Ok(tx_trace)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(pbcodec::Block {
            ver: 2,
            hash: prefix_hex::decode(value.header.hash.clone())?,
            number: value.header.number,
            size: value.header.size,
            header: Some(pbcodec::BlockHeader::try_from(value.header)?),
            uncles: vec![],
            transaction_traces,
            balance_changes: vec![],
            code_changes: vec![],
        })
    }
}
