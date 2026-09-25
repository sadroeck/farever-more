//! Typed access to the inter-add-on message bus.

use crate::__wit::farever::addon::bus as raw;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: u64,
    pub monotonic_ms: u64,
    pub source_addon_id: String,
    pub topic: String,
    pub correlation_id: Option<u64>,
    pub payload: Vec<u8>,
}

impl From<raw::AddonMessage> for Message {
    fn from(value: raw::AddonMessage) -> Self {
        Self {
            id: value.id,
            monotonic_ms: value.monotonic_ms,
            source_addon_id: value.source_addon_id,
            topic: value.topic,
            correlation_id: value.correlation_id,
            payload: value.payload,
        }
    }
}

pub struct Messages {
    dropped_before: u64,
    messages: Vec<Message>,
}

impl Messages {
    #[doc(hidden)]
    pub fn from_raw(dropped_before: u64, messages: Vec<raw::AddonMessage>) -> Self {
        Self {
            dropped_before,
            messages: messages.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn dropped_before(&self) -> u64 {
        self.dropped_before
    }

    pub fn iter(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter()
    }

    #[must_use]
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Subscribers,
    Addon(String),
}

impl From<Target> for raw::MessageTarget {
    fn from(value: Target) -> Self {
        match value {
            Target::Subscribers => Self::Subscribers,
            Target::Addon(id) => Self::Addon(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidTopic,
    PayloadTooLarge,
    TooManySubscriptions,
    QuotaExceeded,
    WrongLifecyclePhase,
}

impl From<raw::MessageError> for Error {
    fn from(value: raw::MessageError) -> Self {
        match value {
            raw::MessageError::InvalidTopic => Self::InvalidTopic,
            raw::MessageError::PayloadTooLarge => Self::PayloadTooLarge,
            raw::MessageError::TooManySubscriptions => Self::TooManySubscriptions,
            raw::MessageError::QuotaExceeded => Self::QuotaExceeded,
            raw::MessageError::WrongLifecyclePhase => Self::WrongLifecyclePhase,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for Error {}

pub struct Subscriptions {
    _private: (),
}

impl Subscriptions {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn subscribe(&self, topic: &str) -> Result<(), Error> {
        raw::subscribe(topic).map_err(Into::into)
    }
}

pub struct Bus {
    _private: (),
}

impl Bus {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn publish(
        &self,
        topic: &str,
        target: Target,
        correlation_id: Option<u64>,
        payload: &[u8],
    ) -> Result<u32, Error> {
        raw::publish(topic, &target.into(), correlation_id, payload).map_err(Into::into)
    }
}
