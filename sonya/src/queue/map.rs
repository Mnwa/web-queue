use crate::queue::connection::BroadcastMessage;
use dashmap::mapref::one::Ref;
use dashmap::DashMap;
use derive_more::{Display, Error, From};
use futures::stream::BoxStream;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sled::{Batch, IVec, Tree};
use sonya_meta::config::Queue as QueueOptions;
use sonya_meta::message::{
    Event, RequestSequence, RequestSequenceId, SequenceEvent, SequenceId, UniqIdEvent,
};
use std::collections::BTreeMap;
use std::fmt::Debug;
use tokio::sync::broadcast::{channel, Receiver, Sender};

pub type QueueMap = sled::Db;

#[derive(Debug)]
pub struct Queue<T> {
    map: QueueMap,
    max_key_updates: Option<usize>,
    queue_meta: DashMap<String, QueueBroadcast<T>>,
}

impl<'a, T> Queue<T>
where
    T: 'a + Send + DeserializeOwned + Serialize + Debug + Event + Clone,
{
    pub fn new(config: QueueOptions) -> QueueResult<Self> {
        let db_config = match config.db_path {
            None => sled::Config::new().temporary(true),
            Some(dp) => sled::Config::new().path(dp).use_compression(true),
        };

        let map = db_config.open()?;

        let this = Self {
            map,
            max_key_updates: config.max_key_updates,
            queue_meta: Default::default(),
        };

        config
            .default
            .into_iter()
            .try_for_each(|q| this.create_queue(q))?;

        Ok(this)
    }

    pub fn create_queue(&self, queue_name: String) -> QueueResult<()> {
        self.map
            .open_tree(queue_name.as_bytes())
            .map(|_| ())
            .map_err(QueueError::from)
    }

    pub fn delete_queue(&self, queue_name: String, id: String) -> QueueResult<()> {
        let queue = get_queue_broadcast(queue_name.clone(), &self.queue_meta);
        queue.keys.remove(&id);

        let mut batch = Batch::default();

        let tree = self.map.open_tree(queue_name.as_bytes())?;

        for response in tree.scan_prefix(id.as_bytes()) {
            let (key, _) = response?;

            batch.remove(key);
        }

        tree.apply_batch(batch).map_err(QueueError::from)
    }

    pub fn subscribe_queue_by_id(
        &self,
        queue_name: String,
        id: String,
        sequence: RequestSequence,
    ) -> QueueResult<Subscription<'a, T>> {
        if !self.check_tree_exists(&queue_name) {
            return Ok(Default::default());
        }

        let tree = self.map.open_tree(queue_name.as_bytes())?;

        let mut prev_items = get_prev_items::<T>(&tree, &id, sequence)?;

        if let Some(last) = prev_items.as_mut().and_then(|vec| vec.last_mut()) {
            last.set_last(true)
        }

        let prev_len = prev_items.as_ref().map(|i| i.len());

        let queue = get_queue_broadcast(queue_name, &self.queue_meta);
        let key_sender = get_key_broadcast(&id, &queue);

        let recv = key_sender.sender.subscribe();

        Ok(Subscription {
            stream: Some(prepare_stream(recv, prev_items)),
            preloaded_count: prev_len,
        })
    }

    pub fn subscribe_queue(
        &self,
        queue_name: String,
        sequence: RequestSequence,
    ) -> QueueResult<Subscription<'a, T>> {
        if !self.check_tree_exists(&queue_name) {
            return Ok(Default::default());
        }

        let tree = self.map.open_tree(queue_name.as_bytes())?;

        let mut prev_items = get_prev_all_items::<T>(&tree, sequence)?;

        if let Some(last) = prev_items.as_mut().and_then(|vec| vec.last_mut()) {
            last.set_last(true)
        }

        let prev_len = prev_items.as_ref().map(|i| i.len());

        let queue = get_queue_broadcast(queue_name, &self.queue_meta);

        let recv = queue.sender.subscribe();

        Ok(Subscription {
            stream: Some(prepare_stream(recv, prev_items)),
            preloaded_count: prev_len,
        })
    }

    pub fn send_to_queue(
        &self,
        queue_name: String,
        mut value: T,
    ) -> QueueResult<(bool, Option<SequenceId>)> {
        if !self.check_tree_exists(&queue_name) {
            return Ok((false, None));
        }

        let id = value.get_id();

        let sequence = match value.get_sequence() {
            None => {
                let id = self.generate_next_id(&queue_name, id)?;

                value.set_sequence(id);

                id.get()
            }
            Some(s) => s.get(),
        };

        if !matches!(self.max_key_updates, Some(0)) {
            let id = get_id(value.get_id(), sequence);

            let tree = self.map.open_tree(queue_name.as_bytes())?;

            tree.insert(id, rmp_serde::to_vec(&value)?)?;

            if let Some(m) = self.max_key_updates {
                let mut batch = Batch::default();

                tree.scan_prefix(value.get_id().as_bytes())
                    .rev()
                    .skip(m - 1)
                    .try_for_each::<_, QueueResult<()>>(|r| {
                        let (k, _) = r?;
                        batch.remove(k);
                        Ok(())
                    })?;

                tree.apply_batch(batch)?;
            }
        }

        let queue = get_queue_broadcast(queue_name, &self.queue_meta);
        let _ = queue.sender.send(value.clone());

        let key_sender = get_key_broadcast(value.get_id(), &queue);
        let _ = key_sender.sender.send(value);

        Ok((true, SequenceId::new(sequence)))
    }

    pub fn close_queue(&self, queue_name: String) -> QueueResult<bool> {
        self.queue_meta.remove(&queue_name);

        self.map.drop_tree(queue_name).map_err(QueueError::from)
    }

    fn check_tree_exists(&self, queue_name: &str) -> bool {
        matches!(
            self.map
                .tree_names()
                .into_iter()
                .find(|v| v == queue_name.as_bytes()),
            Some(_)
        )
    }

    fn generate_next_id(&self, queue_name: &str, id: &str) -> QueueResult<SequenceId> {
        let mut key = Vec::from("id_");
        key.extend_from_slice(queue_name.as_bytes());
        key.extend_from_slice(id.as_bytes());

        let res = self.map.update_and_fetch(key, |v| {
            v.and_then(|v| Some(u64::from_be_bytes(v.try_into().ok()?)))
                .and_then(|id| id.checked_add(1))
                .map(|id| IVec::from(&id.to_be_bytes()))
                .unwrap_or_else(|| IVec::from(&1u64.to_be_bytes()))
                .into()
        })?;

        res.and_then(|r| Some(u64::from_be_bytes(r.as_ref().try_into().ok()?)))
            .and_then(SequenceId::new)
            .map(Ok)
            .unwrap_or_else(|| Err(QueueError::ZeroSequence))
    }
}

fn prepare_stream<'a, T: 'a + DeserializeOwned + Send + Clone>(
    mut receiver: Receiver<T>,
    prev_items: Option<Vec<T>>,
) -> BoxStream<'a, BroadcastMessage<T>> {
    Box::pin(async_stream::stream! {
        if let Some(pi) = prev_items {
            let mut iter = pi.into_iter();
            while let Some(e) = iter.next() {
                yield BroadcastMessage::Message(e)
            }
        }
        while let Ok(value) = receiver.recv().await {
            yield BroadcastMessage::Message(value)
        }
    })
}

fn get_id(id: &str, sequence: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(id.as_bytes().len() + std::mem::size_of::<SequenceId>());
    key.extend_from_slice(id.as_bytes());
    key.extend_from_slice(&sequence.to_be_bytes());

    key
}

fn get_prev_items<T: DeserializeOwned>(
    tree: &Tree,
    id: &str,
    sequence: RequestSequence,
) -> QueueResult<Option<Vec<T>>> {
    sequence
        .map(|sequence_id| {
            extract_sequences(tree, sequence_id, id)
                .map(|r| {
                    r.map(|(_, v)| v)
                        .map_err(QueueError::from)
                        .and_then(|v| rmp_serde::from_slice(&v).map_err(QueueError::from))
                })
                .collect()
        })
        .transpose()
}

fn extract_sequences(
    tree: &Tree,
    sequence_id: RequestSequenceId,
    id: &str,
) -> Box<dyn Iterator<Item = sled::Result<(IVec, IVec)>>> {
    match sequence_id {
        RequestSequenceId::Id(s) => Box::new(tree.range(get_id(id, s.get())..get_id(id, u64::MAX))),
        RequestSequenceId::Last => Box::new(tree.scan_prefix(id.as_bytes()).rev().take(1)),
        RequestSequenceId::First => Box::new(tree.scan_prefix(id.as_bytes())),
    }
}

fn get_prev_all_items<T: DeserializeOwned + SequenceEvent + UniqIdEvent>(
    tree: &Tree,
    sequence: RequestSequence,
) -> QueueResult<Option<Vec<T>>> {
    sequence
        .map(|sequence_id| {
            let i = tree.iter().values().map(|v| {
                v.map_err(QueueError::from)
                    .and_then(|v| rmp_serde::from_slice(&v).map_err(QueueError::from))
            });

            let i: Box<dyn Iterator<Item = Result<T, QueueError>>> = match sequence_id {
                RequestSequenceId::Id(s) => {
                    Box::new(i.filter(move |v: &Result<T, QueueError>| match v {
                        Ok(v) => v.get_sequence().filter(|cs| *cs >= s).is_some(),
                        Err(_) => true,
                    }))
                }
                RequestSequenceId::Last => {
                    let mut map: BTreeMap<String, T> = BTreeMap::new();

                    for item in i {
                        match item {
                            Ok(v) => {
                                map.insert(v.get_id().to_string(), v);
                            }
                            e @ Err(_) => return e.map(|r| vec![r]),
                        }
                    }

                    Box::new(map.into_values().map(Ok))
                }
                RequestSequenceId::First => Box::new(i),
            };

            i.collect::<Result<Vec<_>, _>>()
        })
        .transpose()
}

#[derive(Debug, Display, From, Error)]
pub enum QueueError {
    Db(sled::Error),
    Encode(rmp_serde::encode::Error),
    Decode(rmp_serde::decode::Error),
    #[display(fmt = "sequence must be more then 0")]
    ZeroSequence,
    #[display(fmt = "these queue name is reserved by system")]
    SystemQueueName,
}

pub type QueueResult<T> = Result<T, QueueError>;

#[derive(Debug)]
struct QueueBroadcast<T> {
    sender: Sender<T>,
    keys: DashMap<String, KeyBroadcast<T>>,
}
#[derive(Debug)]
struct KeyBroadcast<T> {
    sender: Sender<T>,
}

// Potentially may be replaced with consistent entry and downgrade
fn get_queue_broadcast<T: Clone>(
    queue_name: String,
    queue_broadcasts: &DashMap<String, QueueBroadcast<T>>,
) -> Ref<'_, String, QueueBroadcast<T>> {
    if !queue_broadcasts.contains_key(&queue_name) {
        queue_broadcasts.insert(
            queue_name.clone(),
            QueueBroadcast {
                sender: channel(1024).0,
                keys: Default::default(),
            },
        );
    }

    queue_broadcasts
        .get(&queue_name)
        .expect("data race occurred, queue broadcast already dropped")
}

// Potentially may be replaced with consistent entry and downgrade
fn get_key_broadcast<'a, T: Clone + SequenceEvent + DeserializeOwned>(
    id: &str,
    queue_broadcast: &'a QueueBroadcast<T>,
) -> Ref<'a, String, KeyBroadcast<T>> {
    if !queue_broadcast.keys.contains_key(id) {
        queue_broadcast.keys.insert(
            id.to_string(),
            KeyBroadcast {
                sender: channel(1024).0,
            },
        );
    }

    queue_broadcast
        .keys
        .get(id)
        .expect("data race occurred, keys broadcast already dropped")
}

pub struct Subscription<'a, T> {
    pub stream: Option<BoxStream<'a, BroadcastMessage<T>>>,
    pub preloaded_count: Option<usize>,
}

impl<'a, T> Default for Subscription<'a, T> {
    fn default() -> Self {
        Self {
            stream: None,
            preloaded_count: None,
        }
    }
}
