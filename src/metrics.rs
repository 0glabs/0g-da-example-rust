use prometheus::{IntCounter, Registry};

fn int_counter<S: Into<String>>(registry: &Registry, name: S, help: S) -> IntCounter {
    let int_counter = Box::new(IntCounter::new(name, help).unwrap());
    registry.register(int_counter.clone()).unwrap();
    *int_counter
}

pub struct DAMetrics {
    pub(crate) dispersed_bytes: IntCounter,
    pub(crate) dispersal_rate_limited: IntCounter,
    pub(crate) confirmed_bytes: IntCounter,
    pub(crate) poll_confirmation_count: IntCounter,
}

impl DAMetrics {
    pub fn new(metrics_registry: &Registry) -> Self {
        Self {
            dispersed_bytes: int_counter(
                metrics_registry,
                "da_dispersed_bytes",
                "Number of bytes dispersed by the DA layer",
            ),
            dispersal_rate_limited: int_counter(
                metrics_registry,
                "da_dispersal_rate_limited",
                "Times the dispersal was rate limited",
            ),
            confirmed_bytes: int_counter(
                metrics_registry,
                "da_confirmed_bytes",
                "Number of bytes confirmed by the DA layer",
            ),
            poll_confirmation_count: int_counter(
                metrics_registry,
                "da_poll_confirmation_count",
                "Number of times we polled the DA for confirmation",
            ),
        }
    }
}
