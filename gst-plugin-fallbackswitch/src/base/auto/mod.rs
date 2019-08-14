mod aggregator;
pub use self::aggregator::AggregatorExt;
pub use self::aggregator::{Aggregator, AggregatorClass, NONE_AGGREGATOR};

mod aggregator_pad;
pub use self::aggregator_pad::AggregatorPadExt;
pub use self::aggregator_pad::{AggregatorPad, AggregatorPadClass, NONE_AGGREGATOR_PAD};

#[doc(hidden)]
pub mod traits {
    pub use super::AggregatorExt;
    pub use super::AggregatorPadExt;
}
