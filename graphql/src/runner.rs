use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::prelude::{QueryExecutionOptions, StoreResolver, SubscriptionExecutionOptions};
use crate::query::execute_query;
use crate::subscription::execute_prepared_subscription;
use graph::prelude::MetricsRegistry;
use graph::prometheus::{Gauge, Histogram};
use graph::{
    components::store::SubscriptionManager,
    prelude::{
        async_trait, o, CheapClone, DeploymentState, GraphQlRunner as GraphQlRunnerTrait, Logger,
        Query, QueryExecutionError, Subscription, SubscriptionError, SubscriptionResult, ENV_VARS,
    },
};
use graph::{data::graphql::effort::LoadManager, prelude::QueryStoreManager};
use graph::{
    data::query::{QueryResults, QueryTarget},
    prelude::QueryStore,
};

pub struct ResultSizeMetrics {
    histogram: Box<Histogram>,
    max_gauge: Box<Gauge>,
}

impl ResultSizeMetrics {
    fn new(registry: Arc<dyn MetricsRegistry>) -> Self {
        // Divide the Histogram into exponentially sized buckets between 1k and 4G
        let bins = (10..32).map(|n| 2u64.pow(n) as f64).collect::<Vec<_>>();
        let histogram = registry
            .new_histogram(
                "query_result_size",
                "the size of the result of successful GraphQL queries (in CacheWeight)",
                bins,
            )
            .unwrap();

        let max_gauge = registry
            .new_gauge(
                "query_result_max",
                "the maximum size of a query result (in CacheWeight)",
                HashMap::new(),
            )
            .unwrap();

        Self {
            histogram,
            max_gauge,
        }
    }

    // Tests need to construct one of these, but normal code doesn't
    #[cfg(debug_assertions)]
    pub fn make(registry: Arc<dyn MetricsRegistry>) -> Self {
        Self::new(registry)
    }

    pub fn observe(&self, size: usize) {
        let size = size as f64;
        self.histogram.observe(size);
        if self.max_gauge.get() < size {
            self.max_gauge.set(size);
        }
    }
}

/// GraphQL runner implementation for The Graph.
pub struct GraphQlRunner<S, SM> {
    logger: Logger,
    store: Arc<S>,
    subscription_manager: Arc<SM>,
    load_manager: Arc<LoadManager>,
    result_size: Arc<ResultSizeMetrics>,
}

#[cfg(debug_assertions)]
lazy_static::lazy_static! {
    // Test only, see c435c25decbc4ad7bbbadf8e0ced0ff2
    pub static ref INITIAL_DEPLOYMENT_STATE_FOR_TESTS: std::sync::Mutex<Option<DeploymentState>> = std::sync::Mutex::new(None);
}

impl<S, SM> GraphQlRunner<S, SM>
where
    S: QueryStoreManager,
    SM: SubscriptionManager,
{
    /// Creates a new query runner.
    pub fn new(
        logger: &Logger,
        store: Arc<S>,
        subscription_manager: Arc<SM>,
        load_manager: Arc<LoadManager>,
        registry: Arc<dyn MetricsRegistry>,
    ) -> Self {
        let logger = logger.new(o!("component" => "GraphQlRunner"));
        let result_size = Arc::new(ResultSizeMetrics::new(registry));
        GraphQlRunner {
            logger,
            store,
            subscription_manager,
            load_manager,
            result_size,
        }
    }

    /// Check if the subgraph state differs from `state` now in a way that
    /// would affect a query that looked at data as fresh as `latest_block`.
    /// If the subgraph did change, return the `Err` that should be sent back
    /// to clients to indicate that condition
    async fn deployment_changed(
        &self,
        store: &dyn QueryStore,
        state: DeploymentState,
        latest_block: u64,
    ) -> Result<(), QueryExecutionError> {
        if ENV_VARS.graphql.allow_deployment_change {
            return Ok(());
        }
        let new_state = store.deployment_state().await?;
        assert!(new_state.reorg_count >= state.reorg_count);
        if new_state.reorg_count > state.reorg_count {
            // One or more reorgs happened; each reorg can't have gone back
            // farther than `max_reorg_depth`, so that querying at blocks
            // far enough away from the previous latest block is fine. Taking
            // this into consideration is important, since most of the time
            // there is only one reorg of one block, and we therefore avoid
            // flagging a lot of queries a bit behind the head
            let n_blocks = new_state.max_reorg_depth * (new_state.reorg_count - state.reorg_count);
            if latest_block + n_blocks as u64 > state.latest_ethereum_block_number as u64 {
                return Err(QueryExecutionError::DeploymentReverted);
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        query: Query,
        target: QueryTarget,
        max_complexity: Option<u64>,
        max_depth: Option<u8>,
        max_first: Option<u32>,
        max_skip: Option<u32>,
        result_size: Arc<ResultSizeMetrics>,
    ) -> Result<QueryResults, QueryResults> {
        // We need to use the same `QueryStore` for the entire query to ensure
        // we have a consistent view if the world, even when replicas, which
        // are eventually consistent, are in use. If we run different parts
        // of the query against different replicas, it would be possible for
        // them to be at wildly different states, and we might unwittingly
        // mix data from different block heights even if no reverts happen
        // while the query is running. `self.store` can not be used after this
        // point, and everything needs to go through the `store` we are
        // setting up here

        let store = self.store.query_store(target.clone(), false).await?;
        let state = store.deployment_state().await?;
        let network = Some(store.network_name().to_string());
        let schema = store.api_schema()?;

        // Test only, see c435c25decbc4ad7bbbadf8e0ced0ff2
        #[cfg(debug_assertions)]
        let state = INITIAL_DEPLOYMENT_STATE_FOR_TESTS
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(state);

        let max_depth = max_depth.unwrap_or(ENV_VARS.graphql.max_depth);
        let query = crate::execution::Query::new(
            &self.logger,
            schema,
            network,
            query,
            max_complexity,
            max_depth,
        )?;
        self.load_manager
            .decide(
                &store.wait_stats().map_err(QueryExecutionError::from)?,
                query.shape_hash,
                query.query_text.as_ref(),
            )
            .to_result()?;
        let by_block_constraint = query.block_constraint()?;
        let mut max_block = 0;
        let mut result: QueryResults = QueryResults::empty();

        // Note: This will always iterate at least once.
        for (bc, (selection_set, error_policy)) in by_block_constraint {
            let resolver = StoreResolver::at_block(
                &self.logger,
                store.cheap_clone(),
                self.subscription_manager.cheap_clone(),
                bc,
                error_policy,
                query.schema.id().clone(),
                result_size.cheap_clone(),
            )
            .await?;
            max_block = max_block.max(resolver.block_number());
            let query_res = execute_query(
                query.clone(),
                Some(selection_set),
                resolver.block_ptr.clone(),
                QueryExecutionOptions {
                    resolver,
                    deadline: ENV_VARS.graphql.query_timeout.map(|t| Instant::now() + t),
                    max_first: max_first.unwrap_or(ENV_VARS.graphql.max_first),
                    max_skip: max_skip.unwrap_or(ENV_VARS.graphql.max_skip),
                    load_manager: self.load_manager.clone(),
                },
            )
            .await;
            result.append(query_res);
        }

        query.log_execution(max_block);
        self.deployment_changed(store.as_ref(), state, max_block as u64)
            .await
            .map_err(QueryResults::from)
            .map(|()| result)
    }
}

#[async_trait]
impl<S, SM> GraphQlRunnerTrait for GraphQlRunner<S, SM>
where
    S: QueryStoreManager,
    SM: SubscriptionManager,
{
    async fn run_query(self: Arc<Self>, query: Query, target: QueryTarget) -> QueryResults {
        self.run_query_with_complexity(
            query,
            target,
            ENV_VARS.graphql.max_complexity,
            Some(ENV_VARS.graphql.max_depth),
            Some(ENV_VARS.graphql.max_first),
            Some(ENV_VARS.graphql.max_skip),
        )
        .await
    }

    async fn run_query_with_complexity(
        self: Arc<Self>,
        query: Query,
        target: QueryTarget,
        max_complexity: Option<u64>,
        max_depth: Option<u8>,
        max_first: Option<u32>,
        max_skip: Option<u32>,
    ) -> QueryResults {
        self.execute(
            query,
            target,
            max_complexity,
            max_depth,
            max_first,
            max_skip,
            self.result_size.cheap_clone(),
        )
        .await
        .unwrap_or_else(|e| e)
    }

    async fn run_subscription(
        self: Arc<Self>,
        subscription: Subscription,
        target: QueryTarget,
    ) -> Result<SubscriptionResult, SubscriptionError> {
        let store = self.store.query_store(target.clone(), true).await?;
        let schema = store.api_schema()?;
        let network = store.network_name().to_string();

        let query = crate::execution::Query::new(
            &self.logger,
            schema,
            Some(network),
            subscription.query,
            ENV_VARS.graphql.max_complexity,
            ENV_VARS.graphql.max_depth,
        )?;

        if let Err(err) = self
            .load_manager
            .decide(
                &store.wait_stats().map_err(QueryExecutionError::from)?,
                query.shape_hash,
                query.query_text.as_ref(),
            )
            .to_result()
        {
            return Err(SubscriptionError::GraphQLError(vec![err]));
        }

        execute_prepared_subscription(
            query,
            SubscriptionExecutionOptions {
                logger: self.logger.clone(),
                store,
                subscription_manager: self.subscription_manager.cheap_clone(),
                timeout: ENV_VARS.graphql.query_timeout,
                max_complexity: ENV_VARS.graphql.max_complexity,
                max_depth: ENV_VARS.graphql.max_depth,
                max_first: ENV_VARS.graphql.max_first,
                max_skip: ENV_VARS.graphql.max_skip,
                result_size: self.result_size.clone(),
            },
        )
    }

    fn load_manager(&self) -> Arc<LoadManager> {
        self.load_manager.clone()
    }
}
