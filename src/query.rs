use core::fmt;
use std::{
    cell::{Ref, RefCell},
    collections::{HashMap, HashSet},
    future::{Future, IntoFuture},
    hash::Hash,
    mem,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use ::warnings::Warning;
use dioxus_lib::prelude::Task;
use dioxus_lib::prelude::*;
use dioxus_lib::signals::{Readable, Writable};
use dioxus_lib::{
    hooks::{use_memo, use_reactive},
    signals::CopyValue,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Notify;
#[cfg(not(target_family = "wasm"))]
use tokio::time;
#[cfg(not(target_family = "wasm"))]
use tokio::time::Instant;
#[cfg(target_family = "wasm")]
use wasmtimer::tokio as time;
#[cfg(target_family = "wasm")]
use web_time::Instant;

pub trait QueryCapability
where
    Self: 'static + Clone + PartialEq + Hash + Eq,
{
    type Ok;
    type Err;
    type Keys: Hash + PartialEq + Clone;

    /// Query logic.
    fn run(&self, keys: &Self::Keys) -> impl Future<Output = Result<Self::Ok, Self::Err>>;

    /// Implement a custom logic to check if this query should be invalidated or not given a [QueryCapability::Keys].
    fn matches(&self, _keys: &Self::Keys) -> bool {
        true
    }

    /// Default stale time for this query type.
    ///
    /// This is how long the data is considered fresh. If a query subscriber is mounted
    /// and the data is stale, it will re-run the query.
    ///
    /// Defaults to [Duration::ZERO], meaning data is marked stale immediately.
    /// You can override this in your implementation to set a custom default, and users
    /// can still override it per-use with [Query::stale_time].
    fn default_stale_time(&self) -> Duration {
        Duration::ZERO
    }

    /// Default clean time for this query type.
    ///
    /// This is how long the data is kept cached after there are no more query subscribers.
    ///
    /// Defaults to `5min`. You can override this in your implementation to set a custom default,
    /// and users can still override it per-use with [Query::clean_time].
    fn default_clean_time(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    /// Default interval time for this query type.
    ///
    /// This is how often the query reruns automatically in the background.
    ///
    /// Defaults to [Duration::MAX], meaning it never re-runs automatically.
    /// You can override this in your implementation to set a custom default, and users
    /// can still override it per-use with [Query::interval_time].
    fn default_interval_time(&self) -> Duration {
        Duration::MAX
    }
}

pub enum QueryStateData<Q: QueryCapability> {
    /// Has not loaded yet.
    Pending,
    /// Is loading and may not have a previous settled value.
    Loading { res: Option<Result<Q::Ok, Q::Err>> },
    /// Is not loading and has a settled value.
    Settled {
        res: Result<Q::Ok, Q::Err>,
        settlement_instant: Instant,
    },
}

impl<Q: QueryCapability> TryFrom<QueryStateData<Q>> for Result<Q::Ok, Q::Err> {
    type Error = ();

    fn try_from(value: QueryStateData<Q>) -> Result<Self, Self::Error> {
        match value {
            QueryStateData::Loading { res: Some(res) } => Ok(res),
            QueryStateData::Settled { res, .. } => Ok(res),
            _ => Err(()),
        }
    }
}

impl<Q> fmt::Debug for QueryStateData<Q>
where
    Q: QueryCapability,
    Q::Ok: fmt::Debug,
    Q::Err: fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("Pending"),
            Self::Loading { res } => write!(f, "Loading {{ {res:?} }}"),
            Self::Settled { res, .. } => write!(f, "Settled {{ {res:?} }}"),
        }
    }
}

impl<Q: QueryCapability> QueryStateData<Q> {
    /// Check if the state is [QueryStateData::Settled] and [Result::Ok].
    pub fn is_ok(&self) -> bool {
        matches!(self, QueryStateData::Settled { res: Ok(_), .. })
    }

    /// Check if the state is [QueryStateData::Settled] and [Result::Err].
    pub fn is_err(&self) -> bool {
        matches!(self, QueryStateData::Settled { res: Err(_), .. })
    }

    /// Check if the state is [QueryStateData::Loading].
    pub fn is_loading(&self) -> bool {
        matches!(self, QueryStateData::Loading { .. })
    }

    /// Check if the state is [QueryStateData::Pending].
    pub fn is_pending(&self) -> bool {
        matches!(self, QueryStateData::Pending)
    }

    /// Check if the state is stale or not, where stale means outdated.
    pub fn is_stale(&self, query: &Query<Q>) -> bool {
        match self {
            QueryStateData::Pending => true,
            QueryStateData::Loading { .. } => true,
            QueryStateData::Settled {
                settlement_instant, ..
            } => Instant::now().duration_since(*settlement_instant) >= query.stale_time,
        }
    }

    /// Get the value as an [Option].
    pub fn ok(&self) -> Option<&Q::Ok> {
        match self {
            Self::Settled { res: Ok(res), .. } => Some(res),
            Self::Loading { res: Some(Ok(res)) } => Some(res),
            _ => None,
        }
    }

    /// Get the value as an [Result] if possible, otherwise it will panic.
    pub fn unwrap(&self) -> &Result<Q::Ok, Q::Err> {
        match self {
            Self::Loading { res: Some(v) } => v,
            Self::Settled { res, .. } => res,
            _ => unreachable!(),
        }
    }

    fn into_loading(self) -> QueryStateData<Q> {
        match self {
            QueryStateData::Pending => QueryStateData::Loading { res: None },
            QueryStateData::Loading { res } => QueryStateData::Loading { res },
            QueryStateData::Settled { res, .. } => QueryStateData::Loading { res: Some(res) },
        }
    }
}

/// Strategy for controlling whether queries transition to Loading state during invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadingStrategy {
    /// Always transition to Loading state when query is invalidated (default behavior).
    /// This causes components to re-render twice: once when entering Loading, once when Settled.
    ///
    /// Example: `Settled("hello") -> Loading("hello") -> Settled("hello")`
    AlwaysShow,

    /// Skip Loading state transition entirely. Query goes directly from Settled to Settled.
    /// This prevents any loading indicators from showing during refetch, reducing re-renders from 2 to 1.
    ///
    /// Example: `Settled("hello") -> Settled("world")`
    Skip,

    /// Reserved for future use. Currently behaves the same as `Skip`.
    /// In the future, this will skip state updates if the result is unchanged (requires PartialEq).
    ///
    /// Example with same result (future): `Settled("hello") -> (no change)`
    /// Example with different result (future): `Settled("hello") -> Settled("world")`
    Memoized,
}

impl Default for LoadingStrategy {
    fn default() -> Self {
        LoadingStrategy::AlwaysShow
    }
}

pub struct QueriesStorage<Q: QueryCapability> {
    storage: CopyValue<HashMap<Query<Q>, QueryData<Q>>>,
}

impl<Q: QueryCapability> Copy for QueriesStorage<Q> {}

impl<Q: QueryCapability> Clone for QueriesStorage<Q> {
    fn clone(&self) -> Self {
        *self
    }
}

struct QuerySuspenseData {
    notifier: Arc<Notify>,
    task: Task,
}

pub struct QueryData<Q: QueryCapability> {
    state: Rc<RefCell<QueryStateData<Q>>>,
    reactive_contexts: Arc<Mutex<HashSet<ReactiveContext>>>,

    suspense_task: Rc<RefCell<Option<QuerySuspenseData>>>,
    interval_task: Rc<RefCell<Option<(Duration, Task)>>>,
    clean_task: Rc<RefCell<Option<Task>>>,
}

impl<Q: QueryCapability> Clone for QueryData<Q> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            reactive_contexts: self.reactive_contexts.clone(),

            suspense_task: self.suspense_task.clone(),
            interval_task: self.interval_task.clone(),
            clean_task: self.clean_task.clone(),
        }
    }
}

/// Builder for invalidating all queries with configurable loading strategy.
/// Implements IntoFuture so it can be used with `.await`.
pub struct InvalidateAll<'a, Q: QueryCapability> {
    storage: QueriesStorage<Q>,
    loading_strategy: LoadingStrategy,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a, Q: QueryCapability> InvalidateAll<'a, Q> {
    /// Configure the loading strategy for this invalidation.
    pub fn loading_strategy(mut self, strategy: LoadingStrategy) -> Self {
        self.loading_strategy = strategy;
        self
    }
}

impl<'a, Q: QueryCapability> IntoFuture for InvalidateAll<'a, Q> {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let storage = self.storage;
            let loading_strategy = self.loading_strategy;

            // Get all the queries
            let matching_queries = storage
                .storage
                .read()
                .clone()
                .into_iter()
                .collect::<Vec<_>>();
            let matching_queries = matching_queries
                .iter()
                .map(|(q, d)| (q, d))
                .collect::<Vec<_>>();

            // Invalidate the queries
            QueriesStorage::run_queries_with_strategy(&matching_queries, loading_strategy).await
        })
    }
}

/// Builder for invalidating matching queries with configurable loading strategy.
/// Implements IntoFuture so it can be used with `.await`.
pub struct InvalidateMatching<'a, Q: QueryCapability> {
    storage: QueriesStorage<Q>,
    matching_keys: Q::Keys,
    loading_strategy: LoadingStrategy,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a, Q: QueryCapability> InvalidateMatching<'a, Q> {
    /// Configure the loading strategy for this invalidation.
    pub fn loading_strategy(mut self, strategy: LoadingStrategy) -> Self {
        self.loading_strategy = strategy;
        self
    }
}

impl<'a, Q: QueryCapability> IntoFuture for InvalidateMatching<'a, Q> {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let storage = self.storage;
            let matching_keys = self.matching_keys;
            let loading_strategy = self.loading_strategy;

            // Get those queries that match
            let mut matching_queries = Vec::new();
            for (query, data) in storage.storage.read().iter() {
                // Check if keys match AND custom matches logic passes
                if query.keys == matching_keys && query.query.matches(&matching_keys) {
                    matching_queries.push((query.clone(), data.clone()));
                }
            }
            let matching_queries = matching_queries
                .iter()
                .map(|(q, d)| (q, d))
                .collect::<Vec<_>>();

            // Invalidate the queries
            QueriesStorage::run_queries_with_strategy(&matching_queries, loading_strategy).await
        })
    }
}

/// Builder for trying to invalidate all queries without panicking if context doesn't exist.
/// Implements IntoFuture so it can be used with `.await`.
pub struct TryInvalidateAll<'a, Q: QueryCapability> {
    storage: Option<QueriesStorage<Q>>,
    loading_strategy: LoadingStrategy,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a, Q: QueryCapability> TryInvalidateAll<'a, Q> {
    /// Configure the loading strategy for this invalidation.
    pub fn loading_strategy(mut self, strategy: LoadingStrategy) -> Self {
        self.loading_strategy = strategy;
        self
    }
}

impl<'a, Q: QueryCapability> IntoFuture for TryInvalidateAll<'a, Q> {
    type Output = bool;
    type IntoFuture = Pin<Box<dyn Future<Output = bool> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Some(storage) = self.storage else {
                return false;
            };
            let loading_strategy = self.loading_strategy;

            // Get all the queries
            let matching_queries = storage
                .storage
                .read()
                .clone()
                .into_iter()
                .collect::<Vec<_>>();
            let matching_queries = matching_queries
                .iter()
                .map(|(q, d)| (q, d))
                .collect::<Vec<_>>();

            // Invalidate the queries
            QueriesStorage::run_queries_with_strategy(&matching_queries, loading_strategy).await;
            true
        })
    }
}

/// Builder for trying to invalidate matching queries without panicking if context doesn't exist.
/// Implements IntoFuture so it can be used with `.await`.
pub struct TryInvalidateMatching<'a, Q: QueryCapability> {
    storage: Option<QueriesStorage<Q>>,
    matching_keys: Q::Keys,
    loading_strategy: LoadingStrategy,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a, Q: QueryCapability> TryInvalidateMatching<'a, Q> {
    /// Configure the loading strategy for this invalidation.
    pub fn loading_strategy(mut self, strategy: LoadingStrategy) -> Self {
        self.loading_strategy = strategy;
        self
    }
}

impl<'a, Q: QueryCapability> IntoFuture for TryInvalidateMatching<'a, Q> {
    type Output = bool;
    type IntoFuture = Pin<Box<dyn Future<Output = bool> + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Some(storage) = self.storage else {
                return false;
            };
            let matching_keys = self.matching_keys;
            let loading_strategy = self.loading_strategy;

            // Get those queries that match
            let mut matching_queries = Vec::new();
            for (query, data) in storage.storage.read().iter() {
                // Check if keys match AND custom matches logic passes
                if query.keys == matching_keys && query.query.matches(&matching_keys) {
                    matching_queries.push((query.clone(), data.clone()));
                }
            }
            let matching_queries = matching_queries
                .iter()
                .map(|(q, d)| (q, d))
                .collect::<Vec<_>>();

            // Invalidate the queries
            QueriesStorage::run_queries_with_strategy(&matching_queries, loading_strategy).await;
            true
        })
    }
}

impl<Q: QueryCapability> QueriesStorage<Q> {
    fn new_in_root() -> Self {
        Self {
            storage: CopyValue::new_in_scope(HashMap::default(), ScopeId::ROOT),
        }
    }

    fn insert_or_get_query(&mut self, query: Query<Q>) -> QueryData<Q> {
        let query_clone = query.clone();
        let mut storage = self.storage.write();

        let query_data = storage.entry(query).or_insert_with(|| QueryData {
            state: Rc::new(RefCell::new(QueryStateData::Pending)),
            reactive_contexts: Arc::default(),
            suspense_task: Rc::default(),
            interval_task: Rc::default(),
            clean_task: Rc::default(),
        });
        let query_data_clone = query_data.clone();

        // Cancel clean task
        if let Some(clean_task) = query_data.clean_task.take() {
            clean_task.cancel();
        }

        // Start an interval task if necessary
        // If multiple queries subscribers use different intervals the interval task
        // will run using the shortest interval
        let interval = query_clone.interval_time;
        let interval_enabled = query_clone.interval_time != Duration::MAX;
        let interval_task = &mut *query_data.interval_task.borrow_mut();

        let create_interval_task = match interval_task {
            None if interval_enabled => true,
            Some((current_interval, current_interval_task)) if interval_enabled => {
                let new_interval_is_shorter = *current_interval > interval;
                if new_interval_is_shorter {
                    current_interval_task.cancel();
                    *interval_task = None;
                }
                new_interval_is_shorter
            }
            _ => false,
        };
        if create_interval_task {
            let task = spawn_forever(async move {
                loop {
                    // Wait as long as the stale time is configured
                    time::sleep(interval).await;

                    // Run the query
                    QueriesStorage::<Q>::run_queries(&[(&query_clone, &query_data_clone)]).await;
                }
            })
            .expect("Failed to spawn interval task.");
            *interval_task = Some((interval, task));
        }

        query_data.clone()
    }

    fn update_tasks(&mut self, query: Query<Q>) {
        let mut storage_clone = self.storage;
        let mut storage = self.storage.write();

        let query_data = match storage.get_mut(&query) {
            Some(data) => data,
            None => {
                // Query was already removed from storage, nothing to do
                return;
            }
        };

        // Cancel interval task
        if let Some((_, interval_task)) = query_data.interval_task.take() {
            interval_task.cancel();
        }

        // Spawn clean up task if there no more reactive contexts
        if query_data.reactive_contexts.lock().unwrap().is_empty() {
            *query_data.clean_task.borrow_mut() = spawn_forever(async move {
                // Wait as long as the stale time is configured
                time::sleep(query.clean_time).await;

                // Finally clear the query
                let mut storage = storage_clone.write();
                storage.remove(&query);
            });
        }
    }

    pub async fn get(get_query: GetQuery<Q>) -> QueryReader<Q> {
        let query: Query<Q> = get_query.into();

        let mut storage = match try_consume_context::<QueriesStorage<Q>>() {
            Some(storage) => storage,
            None => provide_root_context(QueriesStorage::<Q>::new_in_root()),
        };

        let query_data = storage
            .storage
            .write()
            .entry(query.clone())
            .or_insert_with(|| QueryData {
                state: Rc::new(RefCell::new(QueryStateData::Pending)),
                reactive_contexts: Arc::default(),
                suspense_task: Rc::default(),
                interval_task: Rc::default(),
                clean_task: Rc::default(),
            })
            .clone();

        // Run the query if the value is stale
        if query_data.state.borrow().is_stale(&query) {
            // Set to Loading
            let res = mem::replace(&mut *query_data.state.borrow_mut(), QueryStateData::Pending)
                .into_loading();
            *query_data.state.borrow_mut() = res;
            for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                reactive_context.mark_dirty();
            }

            // Run
            let res = query.query.run(&query.keys).await;

            // Set to Settled
            *query_data.state.borrow_mut() = QueryStateData::Settled {
                res,
                settlement_instant: Instant::now(),
            };
            for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                reactive_context.mark_dirty();
            }

            // Notify the suspense task if any
            if let Some(suspense_task) = &*query_data.suspense_task.borrow() {
                suspense_task.notifier.notify_waiters();
            };
        }

        // Spawn clean up task if there no more reactive contexts
        if query_data.reactive_contexts.lock().unwrap().is_empty() {
            *query_data.clean_task.borrow_mut() = spawn_forever(async move {
                // Wait as long as the stale time is configured
                time::sleep(query.clean_time).await;

                // Finally clear the query
                let mut storage = storage.storage.write();
                storage.remove(&query);
            });
        }

        QueryReader {
            state: query_data.state,
        }
    }

    /// Invalidate all queries in storage.
    ///
    /// Returns a builder that implements IntoFuture, so you can `.await` it or
    /// configure the loading strategy with `.loading_strategy()` before awaiting.
    ///
    /// # Example
    /// ```ignore
    /// // Simple usage (backward compatible)
    /// QueriesStorage::<MyQuery>::invalidate_all().await;
    ///
    /// // With custom loading strategy
    /// QueriesStorage::<MyQuery>::invalidate_all()
    ///     .loading_strategy(LoadingStrategy::Skip)
    ///     .await;
    /// ```
    pub fn invalidate_all() -> InvalidateAll<'static, Q> {
        let storage = consume_context::<QueriesStorage<Q>>();
        InvalidateAll {
            storage,
            loading_strategy: LoadingStrategy::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Invalidate queries matching the given keys.
    ///
    /// Returns a builder that implements IntoFuture, so you can `.await` it or
    /// configure the loading strategy with `.loading_strategy()` before awaiting.
    ///
    /// # Example
    /// ```ignore
    /// // Simple usage (backward compatible)
    /// QueriesStorage::<MyQuery>::invalidate_matching(keys).await;
    ///
    /// // With custom loading strategy
    /// QueriesStorage::<MyQuery>::invalidate_matching(keys)
    ///     .loading_strategy(LoadingStrategy::Memoized)
    ///     .await;
    /// ```
    pub fn invalidate_matching(matching_keys: Q::Keys) -> InvalidateMatching<'static, Q> {
        let storage = consume_context::<QueriesStorage<Q>>();
        InvalidateMatching {
            storage,
            matching_keys,
            loading_strategy: LoadingStrategy::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Try to invalidate all queries in storage without panicking if context doesn't exist.
    ///
    /// Returns a builder that implements IntoFuture, so you can `.await` it or
    /// configure the loading strategy with `.loading_strategy()` before awaiting.
    /// The future returns `true` if the context was found and queries were invalidated, `false` otherwise.
    ///
    /// # Example
    /// ```ignore
    /// // Simple usage (backward compatible)
    /// let success = QueriesStorage::<MyQuery>::try_invalidate_all().await;
    ///
    /// // With custom loading strategy
    /// let success = QueriesStorage::<MyQuery>::try_invalidate_all()
    ///     .loading_strategy(LoadingStrategy::Skip)
    ///     .await;
    /// ```
    pub fn try_invalidate_all() -> TryInvalidateAll<'static, Q> {
        let storage = try_consume_context::<QueriesStorage<Q>>();
        TryInvalidateAll {
            storage,
            loading_strategy: LoadingStrategy::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Try to invalidate queries matching the given keys without panicking if context doesn't exist.
    ///
    /// Returns a builder that implements IntoFuture, so you can `.await` it or
    /// configure the loading strategy with `.loading_strategy()` before awaiting.
    /// The future returns `true` if the context was found and queries were invalidated, `false` otherwise.
    ///
    /// # Example
    /// ```ignore
    /// // Simple usage (backward compatible)
    /// let success = QueriesStorage::<MyQuery>::try_invalidate_matching(keys).await;
    ///
    /// // With custom loading strategy
    /// let success = QueriesStorage::<MyQuery>::try_invalidate_matching(keys)
    ///     .loading_strategy(LoadingStrategy::Memoized)
    ///     .await;
    /// ```
    pub fn try_invalidate_matching(matching_keys: Q::Keys) -> TryInvalidateMatching<'static, Q> {
        let storage = try_consume_context::<QueriesStorage<Q>>();
        TryInvalidateMatching {
            storage,
            matching_keys,
            loading_strategy: LoadingStrategy::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get the cached query value without running the query function.
    ///
    /// Returns `None` if the query is not in the cache or hasn't been settled yet.
    /// This is useful for reading cached values without triggering a query execution.
    ///
    /// # Example
    /// ```ignore
    /// // Check if we have a cached value
    /// if let Some(cached_value) = QueriesStorage::<MyQuery>::get_query_value(
    ///     Query::new(keys, query)
    /// ) {
    ///     // Use the cached value
    /// }
    /// ```
    pub fn get_query_value(query: Query<Q>) -> Option<Result<Q::Ok, Q::Err>>
    where
        Q::Ok: Clone,
        Q::Err: Clone,
    {
        let storage = consume_context::<QueriesStorage<Q>>();

        // Get the query data if it exists in storage
        let query_data = storage.storage.peek_unchecked().get(&query).cloned()?;

        // Return the value if it's settled
        let state = query_data.state.borrow();
        match &*state {
            QueryStateData::Settled { res, .. } => Some(res.clone()),
            QueryStateData::Loading { res: Some(res) } => Some(res.clone()),
            _ => None,
        }
    }

    /// Try to get the cached query value without panicking if context doesn't exist.
    /// Returns `None` if the context doesn't exist, the query is not cached, or not settled.
    pub fn try_get_query_value(query: Query<Q>) -> Option<Result<Q::Ok, Q::Err>>
    where
        Q::Ok: Clone,
        Q::Err: Clone,
    {
        let storage = try_consume_context::<QueriesStorage<Q>>()?;

        // Get the query data if it exists in storage
        let query_data = storage.storage.peek_unchecked().get(&query).cloned()?;

        // Return the value if it's settled
        let state = query_data.state.borrow();
        match &*state {
            QueryStateData::Settled { res, .. } => Some(res.clone()),
            QueryStateData::Loading { res: Some(res) } => Some(res.clone()),
            _ => None,
        }
    }

    /// Set the query value in-memory without running the query function.
    ///
    /// This is useful for optimistic updates where you want to immediately update
    /// the cached value before the actual query runs. The value will be set as
    /// a settled state with the current timestamp.
    ///
    /// # Example
    /// ```ignore
    /// // Optimistically update the cache
    /// QueriesStorage::<MyQuery>::set_query_value(
    ///     Query::new(keys, query),
    ///     Ok(new_value)
    /// );
    ///
    /// // Later, invalidate to fetch the real value from the server
    /// QueriesStorage::<MyQuery>::invalidate_matching(keys).await;
    /// ```
    pub fn set_query_value(query: Query<Q>, value: Result<Q::Ok, Q::Err>) {
        let storage = consume_context::<QueriesStorage<Q>>();

        // Get the query data if it exists in storage
        let query_data = storage
            .storage
            .peek_unchecked()
            .get(&query)
            .cloned()
            .unwrap();

        // Set the state to Settled with the provided value
        *query_data.state.borrow_mut() = QueryStateData::Settled {
            res: value,
            settlement_instant: Instant::now(),
        };

        // Mark all reactive contexts as dirty to trigger re-renders
        for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
            reactive_context.mark_dirty();
        }
    }

    /// Try to set the query value without panicking if context doesn't exist.
    /// Returns `true` if the value was set successfully, `false` if the context or query doesn't exist.
    pub fn try_set_query_value(query: Query<Q>, value: Result<Q::Ok, Q::Err>) -> bool {
        let Some(storage) = try_consume_context::<QueriesStorage<Q>>() else {
            return false;
        };

        // Get the query data if it exists in storage
        let Some(query_data) = storage.storage.peek_unchecked().get(&query).cloned() else {
            return false;
        };

        // Set the state to Settled with the provided value
        *query_data.state.borrow_mut() = QueryStateData::Settled {
            res: value,
            settlement_instant: Instant::now(),
        };

        // Mark all reactive contexts as dirty to trigger re-renders
        for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
            reactive_context.mark_dirty();
        }

        true
    }

    async fn run_queries_with_strategy(
        queries: &[(&Query<Q>, &QueryData<Q>)],
        loading_strategy: LoadingStrategy,
    ) {
        let tasks = FuturesUnordered::new();

        for (query, query_data) in queries {
            // Determine if we should transition to Loading state based on strategy
            let should_show_loading = match loading_strategy {
                LoadingStrategy::AlwaysShow => true,
                LoadingStrategy::Skip | LoadingStrategy::Memoized => false,
            };

            if should_show_loading {
                // Set to Loading
                let res =
                    mem::replace(&mut *query_data.state.borrow_mut(), QueryStateData::Pending)
                        .into_loading();
                *query_data.state.borrow_mut() = res;
                for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                    reactive_context.mark_dirty();
                }
            }

            tasks.push(Box::pin(async move {
                // Run
                let new_res = query.query.run(&query.keys).await;

                // For Memoized strategy, we'd need PartialEq to compare results.
                // Since we can't guarantee that at compile time, Memoized behaves like Skip for now.
                // Users can use query.loading_strategy on individual queries if they need memoization
                // on types that implement PartialEq.

                // Set to settled
                *query_data.state.borrow_mut() = QueryStateData::Settled {
                    res: new_res,
                    settlement_instant: Instant::now(),
                };
                for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                    reactive_context.mark_dirty();
                }

                // Notify the suspense task if any
                if let Some(suspense_task) = &*query_data.suspense_task.borrow() {
                    suspense_task.notifier.notify_waiters();
                };
            }));
        }

        tasks.count().await;
    }

    async fn run_queries(queries: &[(&Query<Q>, &QueryData<Q>)]) {
        let tasks = FuturesUnordered::new();

        for (query, query_data) in queries {
            // Set to Loading
            let res = mem::replace(&mut *query_data.state.borrow_mut(), QueryStateData::Pending)
                .into_loading();
            *query_data.state.borrow_mut() = res;
            for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                reactive_context.mark_dirty();
            }

            tasks.push(Box::pin(async move {
                // Run
                let res = query.query.run(&query.keys).await;

                // Set to settled
                *query_data.state.borrow_mut() = QueryStateData::Settled {
                    res,
                    settlement_instant: Instant::now(),
                };
                for reactive_context in query_data.reactive_contexts.lock().unwrap().iter() {
                    reactive_context.mark_dirty();
                }

                // Notify the suspense task if any
                if let Some(suspense_task) = &*query_data.suspense_task.borrow() {
                    suspense_task.notifier.notify_waiters();
                };
            }));
        }

        tasks.count().await;
    }
}

pub struct GetQuery<Q: QueryCapability> {
    query: Q,
    keys: Q::Keys,

    stale_time: Duration,
    clean_time: Duration,
    loading_strategy: LoadingStrategy,
}

impl<Q: QueryCapability> GetQuery<Q> {
    pub fn new(keys: Q::Keys, query: Q) -> Self {
        let stale_time = query.default_stale_time();
        let clean_time = query.default_clean_time();
        Self {
            query,
            keys,
            stale_time,
            clean_time,
            loading_strategy: LoadingStrategy::default(),
        }
    }
    /// For how long is the data considered stale. If a query subscriber is mounted and the data is stale, it will re run the query.
    ///
    /// Defaults to [Duration::ZERO], meaning it is marked stale immediately.
    pub fn stale_time(self, stale_time: Duration) -> Self {
        Self { stale_time, ..self }
    }

    /// For how long the data is kept cached after there are no more query subscribers.
    ///
    /// Defaults to [Duration::ZERO], meaning it clears automatically.
    pub fn clean_time(self, clean_time: Duration) -> Self {
        Self { clean_time, ..self }
    }

    /// Set the loading strategy for this query.
    ///
    /// Controls whether the query transitions to Loading state during invalidation.
    /// Defaults to [LoadingStrategy::AlwaysShow].
    pub fn loading_strategy(self, loading_strategy: LoadingStrategy) -> Self {
        Self {
            loading_strategy,
            ..self
        }
    }
}

impl<Q: QueryCapability> From<GetQuery<Q>> for Query<Q> {
    fn from(value: GetQuery<Q>) -> Self {
        Query {
            query: value.query,
            keys: value.keys,

            enabled: true,

            stale_time: value.stale_time,
            clean_time: value.clean_time,
            interval_time: Duration::MAX,
            loading_strategy: value.loading_strategy,
        }
    }
}
#[derive(PartialEq, Clone)]
pub struct Query<Q: QueryCapability> {
    query: Q,
    keys: Q::Keys,

    enabled: bool,

    stale_time: Duration,
    clean_time: Duration,
    interval_time: Duration,
    loading_strategy: LoadingStrategy,
}

impl<Q: QueryCapability> Eq for Query<Q> {}
impl<Q: QueryCapability> Hash for Query<Q> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.query.hash(state);
        self.keys.hash(state);

        self.enabled.hash(state);

        self.stale_time.hash(state);
        self.clean_time.hash(state);
        self.loading_strategy.hash(state);

        // Intentionally left out as intervals can vary from one query subscriber to another
        // self.interval_time.hash(state);
    }
}

impl<Q: QueryCapability> Query<Q> {
    pub fn new(keys: Q::Keys, query: Q) -> Self {
        let stale_time = query.default_stale_time();
        let clean_time = query.default_clean_time();
        let interval_time = query.default_interval_time();
        Self {
            query,
            keys,
            enabled: true,
            stale_time,
            clean_time,
            interval_time,
            loading_strategy: LoadingStrategy::default(),
        }
    }

    /// Enable or disable this query so that it doesnt automatically run.
    ///
    /// Defaults to `true`.
    pub fn enable(self, enabled: bool) -> Self {
        Self { enabled, ..self }
    }

    /// For how long is the data considered stale. If a query subscriber is mounted and the data is stale, it will re run the query
    /// otherwise it return the cached data.
    ///
    /// Defaults to [Duration::ZERO], meaning it is marked stale immediately after it has been used.
    pub fn stale_time(self, stale_time: Duration) -> Self {
        Self { stale_time, ..self }
    }

    /// For how long the data is kept cached after there are no more query subscribers.
    ///
    /// Defaults to `5min`, meaning it clears automatically after 5 minutes of no subscribers to it.
    pub fn clean_time(self, clean_time: Duration) -> Self {
        Self { clean_time, ..self }
    }

    /// Every how often the query reruns.
    ///
    /// Defaults to [Duration::MAX], meaning it never re runs automatically.
    ///
    /// **Note**: If multiple subscribers of the same query use different intervals, only the shortest one will be used.
    pub fn interval_time(self, interval_time: Duration) -> Self {
        Self {
            interval_time,
            ..self
        }
    }

    /// Set the loading strategy for this query.
    ///
    /// Controls whether the query transitions to Loading state during invalidation.
    /// Defaults to [LoadingStrategy::AlwaysShow].
    pub fn loading_strategy(self, loading_strategy: LoadingStrategy) -> Self {
        Self {
            loading_strategy,
            ..self
        }
    }
}

pub struct QueryReader<Q: QueryCapability> {
    state: Rc<RefCell<QueryStateData<Q>>>,
}

impl<Q: QueryCapability> QueryReader<Q> {
    pub fn state(&self) -> Ref<'_, QueryStateData<Q>> {
        self.state.borrow()
    }

    /// Get the result of the query.
    ///
    /// **This method will panic if the query is not settled.**
    pub fn as_settled(&self) -> Ref<'_, Result<Q::Ok, Q::Err>> {
        Ref::map(self.state.borrow(), |state| match state {
            QueryStateData::Settled { res, .. } => res,
            _ => panic!("Query is not settled."),
        })
    }
}

pub struct UseQuery<Q: QueryCapability> {
    query: Memo<Query<Q>>,
}

impl<Q: QueryCapability> Clone for UseQuery<Q> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Q: QueryCapability> Copy for UseQuery<Q> {}

impl<Q: QueryCapability> UseQuery<Q> {
    /// Read the [Query] state.
    ///
    /// This **will** automatically subscribe.
    /// If you want a **non-subscribing** method have a look at [UseQuery::peek].
    pub fn read(&self) -> QueryReader<Q> {
        let storage = consume_context::<QueriesStorage<Q>>();

        // Read the query FIRST to avoid borrow conflicts
        // (reading may trigger recompute which needs to write to storage)
        let query = self.query.read().clone();

        let query_data = storage
            .storage
            .peek_unchecked()
            .get(&query)
            .cloned()
            .unwrap();

        // Subscribe if possible
        if let Some(reactive_context) = ReactiveContext::current() {
            reactive_context.subscribe(query_data.reactive_contexts);
        }

        QueryReader {
            state: query_data.state,
        }
    }

    /// Read the [Query] state.
    ///
    /// This **will not** automatically subscribe.
    /// If you want a **subscribing** method have a look at [UseQuery::read].
    pub fn peek(&self) -> QueryReader<Q> {
        let storage = consume_context::<QueriesStorage<Q>>();
        let query_data = storage
            .storage
            .peek_unchecked()
            .get(&self.query.peek())
            .cloned()
            .unwrap();

        QueryReader {
            state: query_data.state,
        }
    }

    /// Suspend this query until it has been **settled**.
    ///
    /// This **will** automatically subscribe.
    pub fn suspend(&self) -> Result<Result<Q::Ok, Q::Err>, RenderError>
    where
        Q::Ok: Clone,
        Q::Err: Clone,
    {
        let _allow_write_in_component_body =
            ::warnings::Allow::new(warnings::signal_write_in_component_body::ID);

        let storage = consume_context::<QueriesStorage<Q>>();

        // Read the query FIRST to avoid borrow conflicts
        let query = self.query.read().clone();

        let mut storage = storage.storage.write_unchecked();
        let query_data = storage.get_mut(&query).unwrap();

        // Subscribe if possible
        if let Some(reactive_context) = ReactiveContext::current() {
            reactive_context.subscribe(query_data.reactive_contexts.clone());
        }

        let state = &*query_data.state.borrow();
        match state {
            QueryStateData::Pending | QueryStateData::Loading { res: None } => {
                let suspense_task_clone = query_data.suspense_task.clone();
                let mut suspense_task = query_data.suspense_task.borrow_mut();
                let QuerySuspenseData { task, .. } = suspense_task.get_or_insert_with(|| {
                    let notifier = Arc::new(Notify::new());
                    let task = spawn({
                        let notifier = notifier.clone();
                        async move {
                            notifier.notified().await;
                            let _ = suspense_task_clone.borrow_mut().take();
                        }
                    });
                    QuerySuspenseData { notifier, task }
                });
                Err(RenderError::Suspended(SuspendedFuture::new(*task)))
            }
            QueryStateData::Settled { res, .. } | QueryStateData::Loading { res: Some(res) } => {
                Ok(res.clone())
            }
        }
    }

    /// Invalidate this query and await its result.
    ///
    /// Uses the loading strategy configured on the query via `.loading_strategy()`.
    ///
    /// For a `sync` version use [UseQuery::invalidate].
    pub async fn invalidate_async(&self) -> QueryReader<Q> {
        let storage = consume_context::<QueriesStorage<Q>>();

        let query = self.query.read().clone();
        let query_data = storage
            .storage
            .peek_unchecked()
            .get(&query)
            .cloned()
            .unwrap();

        let loading_strategy = query.loading_strategy;

        // Run the query with the configured loading strategy
        QueriesStorage::run_queries_with_strategy(&[(&query, &query_data)], loading_strategy)
            .await;

        QueryReader {
            state: query_data.state.clone(),
        }
    }

    /// Invalidate this query in the background.
    ///
    /// Uses the loading strategy configured on the query via `.loading_strategy()`.
    ///
    /// For an `async` version use [UseQuery::invalidate_async].
    pub fn invalidate(&self) {
        let storage = consume_context::<QueriesStorage<Q>>();

        let query = self.query.read().clone();
        let query_data = storage
            .storage
            .peek_unchecked()
            .get(&query)
            .cloned()
            .unwrap();

        let loading_strategy = query.loading_strategy;

        // Run the query with the configured loading strategy
        spawn(async move {
            QueriesStorage::run_queries_with_strategy(&[(&query, &query_data)], loading_strategy)
                .await
        });
    }
}

/// Queries are used to get data asynchronously (e.g external resources such as HTTP APIs), which can later be cached or refreshed.
///
/// Important concepts:
///
/// ### Stale time
/// This is how long will a value that is cached, considered to be recent enough.
/// So in other words, if a value is stale it means that its outdated and therefore it should be refreshed.
///
/// By default the stale time is `0ms`, so if a value is cached and a new query subscriber
/// is interested in this value, it will get refreshed automatically.
///
/// See [Query::stale_time].
///
/// ### Clean time
/// This is how long will a value kept cached after there are no more subscribers of that query.
///
/// Imagine there is `Subscriber 1` of a query, the data is requested and cached.
/// But after some seconds the `Subscriber 1` is unmounted, but the data is not cleared as the default clean time is `5min`.
/// A few seconds later the `Subscriber 1` gets mounted again, it requests the data again but this time it is returned directly from the cache.
///
/// See [Query::clean_time].
///
/// ### Interval time
/// This is how often do you want a query to be refreshed in the background automatically.
/// By default it never refreshes automatically.
///
/// See [Query::interval_time].
pub fn use_query<Q: QueryCapability>(query: Query<Q>) -> UseQuery<Q> {
    let mut storage = match try_consume_context::<QueriesStorage<Q>>() {
        Some(storage) => storage,
        None => provide_root_context(QueriesStorage::<Q>::new_in_root()),
    };

    let current_query = use_hook(|| Rc::new(RefCell::new(None)));

    let query = use_memo(use_reactive!(|query| {
        let query_data = storage.insert_or_get_query(query.clone());

        // Update the query tasks if there has been a change in the query
        if let Some(prev_query) = current_query.borrow_mut().take() {
            storage.update_tasks(prev_query);
        }

        // Store this new query
        current_query.borrow_mut().replace(query.clone());

        // Immediately run the query if enabled and the value is stale
        if query.enabled && query_data.state.borrow().is_stale(&query) {
            let query = query.clone();
            spawn(async move {
                QueriesStorage::run_queries(&[(&query, &query_data)]).await;
            });
        }

        query
    }));

    // Update the query tasks when the scope is dropped
    use_drop({
        move || {
            storage.update_tasks(query.peek().clone());
        }
    });

    UseQuery { query }
}
