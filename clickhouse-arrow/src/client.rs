/// The `client` module provides the primary interface for interacting with `ClickHouse`
/// over its native protocol, with full support for Apache Arrow interoperability.
/// The main entry point is the [`Client`] struct, which supports both native `ClickHouse`
/// data formats ([`NativeClient`]) and Arrow-compatible formats ([`ArrowClient`]).
///
/// This module is designed to be thread-safe, with [`Client`] instances that can be
/// cloned and shared across threads. It supports querying, inserting data, managing
/// database schemas, and handling `ClickHouse` events like progress and profiling.
mod builder;
mod chunk;
#[cfg(feature = "cloud")]
mod cloud;
pub(crate) mod connection;
mod internal;
mod options;
mod reader;
mod response;
mod tcp;
mod writer;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;

use arrow::array::{ArrayRef, RecordBatch};
use arrow::compute::take_record_batch;
use arrow::datatypes::SchemaRef;
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use strum::AsRefStr;
use tokio::sync::{broadcast, mpsc, oneshot};

pub use self::builder::*;
pub use self::connection::ConnectionStatus;
pub(crate) use self::internal::{Message, Operation};
pub use self::options::*;
pub use self::response::*;
pub use self::tcp::Destination;
use crate::arrow::utils::batch_to_rows;
use crate::constants::*;
use crate::formats::{ClientFormat, NativeFormat};
use crate::native::block::Block;
use crate::native::block_info::BlockInfo;
use crate::native::protocol::{CompressionMethod, ProfileEvent};
use crate::prelude::*;
use crate::query::{ParsedQuery, QueryParams};
use crate::schema::CreateOptions;
use crate::{Error, Progress, Result, Row};
// use std::str::FromStr; // no longer needed with header-driven serialization

static CLIENT_ID: AtomicU16 = AtomicU16::new(0);

/// A `ClickHouse` client configured for the native format.
///
/// This type alias provides a client that works with `ClickHouse`'s internal
/// data representation, useful when you need direct control over types
/// and don't require Arrow integration.
pub type NativeClient = Client<NativeFormat>;

/// A `ClickHouse` client configured for Apache Arrow format.
///
/// This type alias provides a client that works with Arrow `RecordBatch`es,
/// enabling seamless integration with the Arrow ecosystem for data processing
/// and analytics workflows.
pub type ArrowClient = Client<ArrowFormat>;

/// Configuration for a `ClickHouse` connection, including tracing and cloud-specific settings.
///
/// This struct is used to pass optional context to [`Client::connect`], enabling features
/// like distributed tracing or cloud instance tracking.
///
/// # Fields
/// - `trace`: Optional tracing context for logging and monitoring.
/// - `cloud`: Optional cloud-specific configuration (requires the `cloud` feature).
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(feature = "cloud"), derive(Copy))]
pub struct ConnectionContext {
    pub trace: Option<TraceContext>,
    #[cfg(feature = "cloud")]
    pub cloud: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Emitted clickhouse events from the underlying connection
#[derive(Debug, Clone)]
pub struct Event {
    pub event:     ClickHouseEvent,
    pub qid:       Qid,
    pub client_id: u16,
}

/// Profile and progress events from clickhouse
#[derive(Debug, Clone, AsRefStr)]
pub enum ClickHouseEvent {
    Progress(Progress),
    Profile(Vec<ProfileEvent>),
}

/// A thread-safe handle for interacting with a `ClickHouse` database over its native protocol.
///
/// The `Client` struct is the primary interface for executing queries, inserting data, and
/// managing database schemas. It supports two data formats:
/// - [`NativeClient`]: Uses `ClickHouse`'s native [`Block`] format for data exchange.
/// - [`ArrowClient`]: Uses Apache Arrow's [`RecordBatch`] for seamless interoperability with Arrow
///   ecosystems.
///
/// `Client` instances are lightweight and can be cloned and shared across threads. Each instance
/// maintains a reference to an underlying connection, which is managed automatically. The client
/// also supports event subscription for receiving progress and profiling information from
/// `ClickHouse`.
///
/// # Usage
/// Create a `Client` using the [`ClientBuilder`] for a fluent configuration experience, or use
/// [`Client::connect`] for direct connection setup.
///
/// # Examples
/// ```rust,ignore
/// use clickhouse_arrow::prelude::*;
/// use clickhouse_arrow::arrow;
/// use futures_util::StreamExt;
///
/// let client = Client::builder()
///     .destination("localhost:9000")
///     .username("default")
///     .build::<ArrowFormat>()
///     .await?;
///
/// // Execute a query
/// let batch = client
///     .query("SELECT 1")
///     .await?
///     .collect::<Vec<_>>()
///     .await
///     .into_iter()
///     .collect::<Result<Vec<_>>>()?;
/// arrow::util::pretty::print_batches(batch)?;
/// ```
#[derive(Clone, Debug)]
pub struct Client<T: ClientFormat> {
    pub client_id: u16,
    connection:    Arc<connection::Connection<T>>,
    events:        Arc<broadcast::Sender<Event>>,
    settings:      Option<Arc<Settings>>,
}

impl<T: ClientFormat> Client<T> {
    /// Get an instance of [`ClientBuilder`] which allows creating a `Client` using a builder
    /// Creates a new [`ClientBuilder`] for configuring and building a `ClickHouse` client.
    ///
    /// This method provides a fluent interface to set up a `Client` with custom connection
    /// parameters, such as the server address, credentials, TLS, and compression. The
    /// builder can create either a single [`Client`] or a connection pool (with the `pool`
    /// feature enabled).
    ///
    /// Use this method when you need fine-grained control over the client configuration.
    /// For simple connections, you can also use [`Client::connect`] directly.
    ///
    /// # Returns
    /// A [`ClientBuilder`] instance ready for configuration.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let builder = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .with_username("default")
    ///     .with_password("");
    /// ```
    pub fn builder() -> ClientBuilder { ClientBuilder::new() }

    /// Establishes a connection to a `ClickHouse` server over TCP, with optional TLS support.
    ///
    /// This method creates a new [`Client`] instance connected to the specified `destination`.
    /// The connection can be configured using [`ClientOptions`], which allows setting parameters
    /// like username, password, TLS, and compression. Optional `settings` can be provided to
    /// customize `ClickHouse` session behavior, and a `context` can be used for tracing or
    /// cloud-specific configurations.
    ///
    /// # Parameters
    /// - `destination`: The `ClickHouse` server address (e.g., `"localhost:9000"` or a
    ///   [`Destination`]).
    /// - `options`: Configuration for the connection, including credentials, TLS, and cloud
    ///   settings.
    /// - `settings`: Optional `ClickHouse` session settings (e.g., query timeouts, max rows).
    /// - `context`: Optional connection context for tracing or cloud-specific behavior.
    ///
    /// # Returns
    /// A [`Result`] containing the connected [`Client`] instance, or an error if the connection
    /// fails.
    ///
    /// # Errors
    /// - Fails if the destination cannot be resolved or the connection cannot be established.
    /// - Fails if authentication or TLS setup encounters an issue.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::client::{Client, ClientOptions};
    ///
    /// let options = ClientOptions::default()
    ///     .username("default")
    ///     .password("")
    ///     .use_tls(false);
    ///
    /// let client = Client::connect("localhost:9000", options, None, None).await?;
    /// ```
    #[instrument(
        level = "debug",
        name = "clickhouse.connect",
        fields(
            db.system = "clickhouse",
            db.format = T::FORMAT,
            network.transport = ?if options.use_tls { "tls" } else { "tcp" }
        ),
        skip_all
    )]
    pub async fn connect<A: Into<Destination>>(
        destination: A,
        options: ClientOptions,
        settings: Option<Arc<Settings>>,
        context: Option<ConnectionContext>,
    ) -> Result<Self> {
        let context = context.unwrap_or_default();
        let trace_ctx = context.trace.unwrap_or_default();
        let _ = trace_ctx.link(&Span::current());

        let client_id = CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Resolve the destination
        let destination: Destination = destination.into();
        let addrs = destination.resolve(options.ipv4_only).await?;

        #[cfg(feature = "cloud")]
        {
            // Ping the cloud instance if requested
            if let Some(domain) = options.domain.as_ref().filter(|_| options.ext.cloud.wakeup) {
                let cloud_track = context.cloud.as_deref();
                Self::ping_cloud(domain, options.ext.cloud.timeout, cloud_track).await;
            }
        }

        if let Some(addr) = addrs.first() {
            let _ = Span::current()
                .record("server.address", tracing::field::debug(&addr.ip()))
                .record("server.port", addr.port());
            debug!(server.address = %addr.ip(), server.port = addr.port(), "Initiating connection");
        }

        let (event_tx, _) = broadcast::channel(EVENTS_CAPACITY);
        let events = Arc::new(event_tx);
        let conn_ev = Arc::clone(&events);

        let conn =
            connection::Connection::connect(client_id, addrs, options, conn_ev, trace_ctx).await?;
        let connection = Arc::new(conn);

        debug!("created connection successfully");

        Ok(Client { client_id, connection, events, settings })
    }

    /// Retrieves the status of the underlying `ClickHouse` connection.
    ///
    /// This method returns the current [`ConnectionStatus`] of the client's connection,
    /// indicating whether it is active, idle, or disconnected. Useful for monitoring
    /// the health of the connection before executing queries.
    ///
    /// # Returns
    /// A [`ConnectionStatus`] enum describing the connection state.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::<ArrowFormat>::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build()
    ///     .await
    ///     .unwrap();
    ///
    /// let status = client.status();
    /// println!("Connection status: {status:?}");
    /// ```
    pub fn status(&self) -> ConnectionStatus { self.connection.status() }

    /// Subscribes to progress and profile events from `ClickHouse` queries.
    ///
    /// This method returns a [`broadcast::Receiver`] that delivers [`Event`] instances
    /// containing progress updates ([`Progress`]) or profiling information ([`ProfileEvent`])
    /// as queries execute. Events are generated asynchronously and can be used to monitor
    /// query execution in real time.
    ///
    /// # Returns
    /// A [`broadcast::Receiver<Event>`] for receiving `ClickHouse` events.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    /// use tokio::sync::broadcast::error::RecvError;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let mut receiver = client.subscribe_events();
    /// let handle = tokio::spawn(async move {
    ///     while let Ok(event) = receiver.recv().await {
    ///         println!("Received event: {:?}", event);
    ///     }
    /// });
    ///
    /// // Execute a query to generate events
    /// client.query("SELECT * FROM large_table").await.unwrap();
    /// ```
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> { self.events.subscribe() }

    /// Checks the health of the underlying `ClickHouse` connection.
    ///
    /// This method verifies that the connection is active and responsive. If `ping` is
    /// `true`, it sends a lightweight ping to the `ClickHouse` server to confirm
    /// connectivity. Otherwise, it checks the connection's internal state.
    ///
    /// # Parameters
    /// - `ping`: If `true`, performs an active ping to the server; if `false`, checks the
    ///   connection state without network activity.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the connection is healthy.
    ///
    /// # Errors
    /// - Fails if the connection is disconnected or unresponsive.
    /// - Fails if the ping operation times out or encounters a network error.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    /// client.health_check(true).await.unwrap();
    /// println!("Connection is healthy!");
    /// ```
    pub async fn health_check(&self, ping: bool) -> Result<()> {
        trace!({ ATT_CID } = self.client_id, "sending health check w/ ping={ping}");
        self.conn().await?.check_connection(ping).await
    }

    /// Shuts down the `ClickHouse` client and closes its connection.
    ///
    /// This method gracefully terminates the underlying connection, ensuring that any
    /// pending operations are completed or canceled. After shutdown, the client cannot
    /// be used for further operations.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the shutdown was successful.
    ///
    /// # Errors
    /// - Fails if the connection cannot be closed due to network issues or internal errors.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    /// client.shutdown().await.unwrap();
    /// println!("Client shut down successfully!");
    /// ```
    pub async fn shutdown(&self) -> Result<()> {
        trace!("shutting down client");
        self.conn().await?.shutdown().await
    }

    /// Inserts a block of data into `ClickHouse` using the native protocol.
    ///
    /// This method sends an insert query with a single block of data, formatted according to
    /// the client's data format (`T: ClientFormat`). For [`NativeClient`], the data is a
    /// [`Block`]; for [`ArrowClient`], it is a [`RecordBatch`]. The query is executed
    /// asynchronously, and any response data, progress events, or errors are streamed back.
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]). The returned stream yields `()` on success or an
    /// error if the insert fails.
    ///
    /// # Parameters
    /// - `query`: The insert query (e.g., `"INSERT INTO my_table VALUES"`).
    /// - `block`: The data to insert, in the format specified by `T` ([`Block`] or
    ///   [`RecordBatch`]).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a stream of [`Result<()>`], where each item indicates
    /// the success or failure of processing response data.
    ///
    /// # Errors
    /// - Fails if the query is malformed or the data format is invalid.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., schema mismatch).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    /// use arrow::record_batch::RecordBatch;
    ///
    /// let client = Client::builder()
    ///     .destination("localhost:9000")
    ///     .build_arrow()
    ///     .await?;
    ///
    /// let qid = Qid::new();
    /// // Assume `batch` is a valid RecordBatch
    /// let batch: RecordBatch = // ...;
    /// let stream = client.insert("INSERT INTO my_table VALUES", batch, Some(qid)).await?;
    /// while let Some(result) = stream.next().await {
    ///     result?; // Check for errors
    /// }
    /// ```
    #[instrument(
        level = "trace",
        name = "clickhouse.insert",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "insert",
            db.format = T::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        ),
    )]
    pub async fn insert(
        &self,
        query: impl Into<ParsedQuery>,
        block: T::Data,
        qid: Option<Qid>,
    ) -> Result<impl Stream<Item = Result<()>> + '_> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);

        // Create metadata + header channels for the query
        let (tx_query, rx_query) = oneshot::channel();
        let (header_tx, header_rx) = oneshot::channel();
        let connection = self.conn().await?;

        // Send query first; also request header so we can wait for it before data
        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    params: None,
                    response: tx_query,
                    header: Some(header_tx),
                },
                qid,
                false,
            )
            .await?;

        // Wait for the server's Header before sending data
        let _header = header_rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive header for query {qid}")))?;

        // Immediately send data block for this INSERT
        let (tx_insert, rx_insert) = oneshot::channel();
        let _ = connection
            .send_operation(Operation::Insert { data: block, response: tx_insert }, qid, true)
            .await?;
        // Await insert ack
        rx_insert.await.map_err(|_| {
            Error::Protocol(format!("Failed to receive response from insert {qid}"))
        })??;

        trace!({ ATT_CID } = self.client_id, { ATT_QID } = %qid, "sent insert data, awaiting query response stream");
        // Now await the query response stream
        let responses = rx_query
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;

        // Decrement load balancer
        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<T::Data>::weight_insert());

        Ok(self.insert_response(responses, qid))
    }

    // insert_with_block_builder is specialized for NativeFormat; see impl Client<NativeFormat>.

    /// Inserts multiple blocks of data into `ClickHouse` using the native protocol.
    ///
    /// This method sends an insert query with a collection of data blocks, formatted
    /// according to the client's data format (`T: ClientFormat`). For [`NativeClient`],
    /// the data is a `Vec<Block>`; for [`ArrowClient`], it is a `Vec<RecordBatch>`.
    /// The query is executed asynchronously, and any response data, progress events,
    /// or errors are streamed back.
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]). The returned stream yields `()` on success or an
    /// error if the insert fails. Use this method when inserting multiple batches of
    /// data to reduce overhead compared to multiple [`Client::insert`] calls.
    ///
    /// # Parameters
    /// - `query`: The insert query (e.g., `"INSERT INTO my_table VALUES"`).
    /// - `batch`: A vector of data blocks to insert, in the format specified by `T`.
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a stream of [`Result<()>`], where each item indicates
    /// the success or failure of processing response data.
    ///
    /// # Errors
    /// - Fails if the query is malformed or any data block is invalid.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., schema mismatch).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    /// use arrow::record_batch::RecordBatch;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `batches` is a Vec<RecordBatch>
    /// let batches: Vec<RecordBatch> = vec![/* ... */];
    /// let stream = client.insert_many("INSERT INTO my_table VALUES", batches, None).await.unwrap();
    /// while let Some(result) = stream.next().await {
    ///     result.unwrap(); // Check for errors
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.insert_many",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.operation = "insert",
            db.format = T::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        ),
    )]
    pub async fn insert_many(
        &self,
        query: impl Into<ParsedQuery>,
        batch: Vec<T::Data>,
        qid: Option<Qid>,
    ) -> Result<impl Stream<Item = Result<()>> + '_> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);

        // Create metadata channel
        let (tx, rx) = oneshot::channel();
        let connection = self.conn().await?;

        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    params: None,
                    response: tx,
                    header: None,
                },
                qid,
                false,
            )
            .await?;

        trace!({ ATT_CID } = self.client_id, { ATT_QID } = %qid, "sent query, awaiting response");
        let responses = rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;

        // Send data
        let (tx, rx) = oneshot::channel();
        let _ = connection
            .send_operation(Operation::InsertMany { data: batch, response: tx }, qid, true)
            .await?;
        rx.await.map_err(|_| {
            Error::Protocol(format!("Failed to receive response from insert {qid}"))
        })??;

        // Decrement load balancer
        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<T::Data>::weight_insert_many());

        Ok(self.insert_response(responses, qid))
    }

    /// Executes a raw `ClickHouse` query and streams raw data in the client's format.
    ///
    /// This method sends a query to `ClickHouse` and returns a stream of raw data blocks
    /// in the format specified by `T: ClientFormat` ([`Block`] for [`NativeClient`],
    /// [`RecordBatch`] for [`ArrowClient`]). It is a low-level method suitable for
    /// custom processing of query results. For higher-level interfaces, consider
    /// [`Client::query`] or [`Client::query_rows`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `qid`: A unique query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a stream of [`Result<T::Data>`], where each item is a
    /// data block or an error.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    /// let qid = Qid::new();
    /// let mut stream = client.query_raw("SELECT * FROM my_table", qid).await.unwrap();
    /// while let Some(block) = stream.next().await {
    ///     let batch = block.unwrap();
    ///     println!("Received batch with {} rows", batch.num_rows());
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = T::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id = %qid
        ),
     )]
    pub async fn query_raw<P: Into<QueryParams>>(
        &self,
        query: String,
        params: Option<P>,
        qid: Qid,
    ) -> Result<impl Stream<Item = Result<T::Data>> + 'static> {
        // Create metadata channel
        let (tx, rx) = oneshot::channel();
        let connection = self.conn().await?;

        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    params: params.map(Into::into),
                    response: tx,
                    header: None,
                },
                qid,
                true,
            )
            .await?;

        trace!({ ATT_CID } = self.client_id, { ATT_QID } = %qid, "sent query, awaiting response");

        let responses = rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;
        trace!({ ATT_CID } = self.client_id, { ATT_QID } = %qid, "sent query, awaiting response");

        // Decrement load balancer
        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<T::Data>::weight_query());

        Ok(create_response_stream::<T>(responses, qid, self.client_id))
    }

    /// Executes a `ClickHouse` query and discards all returned data.
    ///
    /// This method sends a query to `ClickHouse` and processes the response stream to
    /// check for errors, but discards any returned data blocks. It is useful for
    /// queries that modify data (e.g., `INSERT`, `UPDATE`, `DELETE`) or DDL statements
    /// where the result data is not needed. For queries that return data, use
    /// [`Client::query`] or [`Client::query_raw`].
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"DROP TABLE my_table"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the query executed successfully.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., permission denied).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// client.execute("DROP TABLE IF EXISTS my_table", None).await.unwrap();
    /// println!("Table dropped successfully!");
    /// ```
    #[instrument(
        name = "clickhouse.execute",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.format = T::FORMAT,
            db.operation = "query",
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn execute(&self, query: impl Into<ParsedQuery>, qid: Option<Qid>) -> Result<()> {
        self.execute_params(query, None::<QueryParams>, qid).await
    }

    /// Executes a `ClickHouse` query with query parameters and discards all returned data.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"DROP TABLE my_table"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the query executed successfully.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., permission denied).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let params = Some(vec![
    ///     ("str", ParamValue::from("hello")),
    ///     ("num", ParamValue::from(42)),
    ///     ("array", ParamValue::from("['a', 'b', 'c']")),
    /// ]);
    /// let query = "SELECT {num:Int64}, {str:String}, {array:Array(String)}";
    /// client.execute_params(query, params, None).await.unwrap();
    /// println!("Table dropped successfully!");
    /// ```
    #[instrument(
        name = "clickhouse.execute_params",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.format = T::FORMAT,
            db.operation = "query",
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn execute_params<P: Into<QueryParams>>(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<P>,
        qid: Option<Qid>,
    ) -> Result<()> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);
        let stream = self.query_raw(query, params, qid).await?;
        tokio::pin!(stream);
        while let Some(next) = stream.next().await {
            drop(next?);
        }
        Ok(())
    }

    /// Executes a `ClickHouse` query without processing the response stream.
    ///
    /// This method sends a query to `ClickHouse` and immediately discards the response
    /// stream without checking for errors or processing data. It is a lightweight
    /// alternative to [`Client::execute`], suitable for fire-and-forget scenarios where
    /// the query's outcome is not critical. For safer execution, use [`Client::execute`].
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"INSERT INTO my_table VALUES (1)"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the query was sent successfully.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    /// client.execute_now("INSERT INTO logs VALUES ('event')", None).await.unwrap();
    /// println!("Log event sent!");
    /// ```
    #[instrument(
        name = "clickhouse.execute_now",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.format = T::FORMAT,
            db.operation = "query",
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn execute_now(&self, query: impl Into<ParsedQuery>, qid: Option<Qid>) -> Result<()> {
        self.execute_now_params(query, None::<QueryParams>, qid).await
    }

    /// Executes a `ClickHouse` query with query parameters without processing the response stream.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"INSERT INTO my_table VALUES (1)"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating whether the query was sent successfully.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build::<ArrowFormat>()
    ///     .await
    ///     .unwrap();
    ///
    ///
    /// let params = Some(vec![("str", ParamValue::from("hello"))]);
    /// let query = "INSERT INTO logs VALUES ({str:String})";
    /// client.execute_now_params(query, params, None).await.unwrap();
    /// println!("Log event sent!");
    /// ```
    #[instrument(
        name = "clickhouse.execute_now_params",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.format = T::FORMAT,
            db.operation = "query",
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn execute_now_params<P: Into<QueryParams>>(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<P>,
        qid: Option<Qid>,
    ) -> Result<()> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);
        drop(self.query_raw(query, params, qid).await?);
        Ok(())
    }

    /// Creates a new database in `ClickHouse` using a DDL statement.
    ///
    /// This method issues a `CREATE DATABASE` statement for the specified database. If no
    /// database is provided, it uses the client's default database from the connection
    /// metadata. The `default` database cannot be created, as it is reserved by `ClickHouse`.
    ///
    /// # Parameters
    /// - `database`: Optional name of the database to create. If `None`, uses the client's default
    ///   database.
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating success or failure of the operation.
    ///
    /// # Errors
    /// - Fails if the database name is invalid or reserved (e.g., `default`).
    /// - Fails if the query execution encounters a `ClickHouse` error.
    /// - Fails if the connection is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::client::{Client, ClientBuilder};
    ///
    /// let client = ClientBuilder::new()
    ///     .destination("localhost:9000")
    ///     .build_native()
    ///     .await?;
    ///
    /// client.create_database(Some("my_db"), None).await?;
    /// ```
    #[instrument(
        name = "clickhouse.create_database",
        skip_all
        fields(db.system = "clickhouse", db.operation = "create.database")
    )]
    pub async fn create_database(&self, database: Option<&str>, qid: Option<Qid>) -> Result<()> {
        let database = database.unwrap_or(self.connection.database());
        let database = database.to_lowercase();
        if &database == "default" {
            warn!("Exiting, cannot create `default` database");
            return Ok(());
        }

        let stmt = create_db_statement(&database)?;
        self.execute(stmt, qid).await?;
        Ok(())
    }

    /// Drops a database in `ClickHouse` using a DDL statement.
    ///
    /// This method issues a `DROP DATABASE` statement for the specified database. The
    /// `default` database cannot be dropped, as it is reserved by `ClickHouse`. If the client
    /// is connected to a non-default database, dropping a different database is not allowed
    /// to prevent accidental data loss.
    ///
    /// # Parameters
    /// - `database`: Name of the database to drop.
    /// - `sync`: If `true`, the operation waits for `ClickHouse` to complete the drop
    ///   synchronously.
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating success or failure of the operation.
    ///
    /// # Errors
    /// - Fails if the database is `default` (reserved).
    /// - Fails if the client is connected to a non-default database different from `database`.
    /// - Fails if the query execution encounters a `ClickHouse` error.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .destination("localhost:9000")
    ///     .database("default") // Must be connected to default to drop 'other' databases
    ///     .build::<NativeFormat>()
    ///     .await?;
    ///
    /// client.drop_database("my_db", true, None).await?;
    /// ```
    #[instrument(
        name = "clickhouse.drop_database",
        skip_all
        fields(db.system = "clickhouse", db.operation = "drop.database")
    )]
    pub async fn drop_database(&self, database: &str, sync: bool, qid: Option<Qid>) -> Result<()> {
        let database = database.to_lowercase();
        if &database == "default" {
            warn!("Exiting, cannot drop `default` database");
            return Ok(());
        }

        // TODO: Should this check remain? Or should the query writing be modified in the case
        // of issuing DDL statements while connected to a non-default database
        let current_database = self.connection.database();
        if current_database != "default"
            && !current_database.is_empty()
            && current_database != database
        {
            error!("Cannot drop database {database} while connected to {current_database}");
            return Err(Error::InsufficientDDLScope(current_database.into()));
        }

        let stmt = drop_db_statement(&database, sync)?;
        self.execute(stmt, qid).await?;
        Ok(())
    }
}

impl<T: ClientFormat> Client<T> {
    /// Get a reference to the underlying connection.
    ///
    /// TODO: Support reconnect.
    #[expect(clippy::unused_async)]
    async fn conn(&self) -> Result<&connection::Connection<T>> {
        // TODO: Add reconnection logic here if configured
        Ok(self.connection.as_ref())
    }

    /// # Feature
    /// Requires the `cloud` feature to be enabled.
    #[cfg(feature = "cloud")]
    #[instrument(level = "trace", name = "clickhouse.cloud.ping")]
    async fn ping_cloud(
        domain: &str,
        timeout: Option<u64>,
        track: Option<&std::sync::atomic::AtomicBool>,
    ) {
        debug!("pinging cloud instance");
        if !domain.is_empty() {
            debug!(domain, "cloud endpoint found");
            // Create receiver channel to cancel ping if dropped
            let (_tx, rx) = oneshot::channel::<()>();
            cloud::ping_cloud(domain.to_string(), timeout, track, rx).await;
        }
    }

    // Helper function to convert a receiver of data into a `ClickHouseResponse`
    fn insert_response(
        &self,
        rx: mpsc::Receiver<Result<T::Data>>,
        qid: Qid,
    ) -> ClickHouseResponse<()> {
        ClickHouseResponse::<()>::from_stream(handle_insert_response::<T>(rx, qid, self.client_id))
    }
}

impl Client<NativeFormat> {
    /// Inserts rows into `ClickHouse` using the native protocol.
    ///
    /// This method sends an insert query with a collection of rows, where each row is
    /// a type `T` implementing [`Row`]. The rows are converted into a `ClickHouse`
    /// [`Block`] and sent over the native protocol. The query is executed asynchronously,
    /// and any response data, progress events, or errors are streamed back.
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]). The returned [`ClickHouseResponse`] yields `()`
    /// on success or an error if the insert fails.
    ///
    /// # Parameters
    /// - `query`: The insert query (e.g., `"INSERT INTO my_table VALUES"`).
    /// - `blocks`: An iterator of rows to insert, where each row implements [`Row`].
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<()>`] that streams the operation's
    /// outcome.
    ///
    /// # Errors
    /// - Fails if the query is malformed or the row data is invalid.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., schema mismatch).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_native()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `MyRow` implements `Row`
    /// let rows = vec![MyRow { /* ... */ }, MyRow { /* ... */ }];
    /// let response = client.insert_rows("INSERT INTO my_table VALUES", rows.into_iter(), None)
    ///     .await
    ///     .unwrap();
    /// while let Some(result) = response.next().await {
    ///     result.unwrap(); // Check for errors
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.insert_rows",
        fields(
            db.system = "clickhouse",
            db.operation = "insert",
            db.format = NativeFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        ),
        skip_all
    )]
    pub async fn insert_rows<T: Row + Send + 'static>(
        &self,
        query: impl Into<ParsedQuery>,
        blocks: impl Iterator<Item = T> + Send + Sync + 'static,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<()>> {
        let cid = self.client_id;
        let (query, qid) = record_query(qid, query.into(), cid);

        // Create metadata channel
        let (tx, rx) = oneshot::channel();
        let (header_tx, header_rx) = oneshot::channel();

        let connection = self.conn().await?;

        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    params: None,
                    response: tx,
                    header: Some(header_tx),
                },
                qid,
                false,
            )
            .await?;

        trace!({ ATT_CID } = cid, { ATT_QID } = %qid, "sent query, awaiting response");
        let responses = rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;

        let header = header_rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive header for query {qid}")))?;
        let data = Block::from_rows(blocks.collect(), header)?;

        let (tx, rx) = oneshot::channel();
        let _ =
            connection.send_operation(Operation::Insert { data, response: tx }, qid, true).await?;
        rx.await.map_err(|_| {
            Error::Protocol(format!("Failed to receive response from insert {qid}"))
        })??;

        // Decrement load balancer
        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<Block>::weight_query());

        Ok(self.insert_response(responses, qid))
    }

    /// Executes a `ClickHouse` query and streams deserialized rows.
    ///
    /// This method sends a query to `ClickHouse` and returns a stream of rows, where
    /// each row is deserialized into type `T` implementing [`Row`]. Rows are grouped
    /// into `ClickHouse` blocks, and the stream yields rows as they are received. Use
    /// this method for type-safe access to query results in native format.
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<T>`] that streams deserialized
    /// rows of type `T`.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if row deserialization fails (e.g., schema mismatch).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_native()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `MyRow` implements `Row`
    /// let mut response = client.query::<MyRow>("SELECT * FROM my_table", None).await.unwrap();
    /// while let Some(row) = response.next().await {
    ///     let row = row.unwrap();
    ///     println!("Row: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query",
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", db.format = NativeFormat::FORMAT)
    )]
    pub async fn query<T: Row + Send + 'static>(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<T>> {
        self.query_params(query, None, qid).await
    }

    /// Executes a `ClickHouse` query with parameters and streams deserialized rows.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<T>`] that streams deserialized
    /// rows of type `T`.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if row deserialization fails (e.g., schema mismatch).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_native()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `MyRow` implements `Row`
    /// let params = Some(vec![("name", ParamValue::from("my_table"))]);
    /// let query = "SELECT * FROM {name:Identifier}";
    /// let mut response = client.query_params::<MyRow>(query, params, None).await.unwrap();
    /// while let Some(row) = response.next().await {
    ///     let row = row.unwrap();
    ///     println!("Row: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_params",
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", db.format = NativeFormat::FORMAT)
    )]
    pub async fn query_params<T: Row + Send + 'static>(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<T>> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);
        let raw = self.query_raw(query, params, qid).await?;
        Ok(ClickHouseResponse::new(Box::pin(raw.flat_map(|block| {
            match block {
                Ok(mut block) => stream::iter(
                    block
                        .take_iter_rows()
                        .filter(|x| !x.is_empty())
                        .map(T::deserialize_row)
                        .map(|maybe| maybe.inspect_err(|error| error!(?error, "deserializing row")))
                        .collect::<Vec<_>>(),
                ),
                Err(e) => stream::iter(vec![Err(e)]),
            }
        }))))
    }

    /// Executes a `ClickHouse` query and returns the first row, discarding the rest.
    ///
    /// This method sends a query to `ClickHouse` and returns the first row deserialized
    /// into type `T` implementing [`Row`], or `None` if the result is empty. It is
    /// useful for queries expected to return a single row (e.g., `SELECT COUNT(*)`).
    /// For streaming multiple rows, use [`Client::query`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT name FROM users WHERE id = 1"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<T>`, where `T` is the deserialized row, or
    /// `None` if no rows are returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if row deserialization fails (e.g., schema mismatch).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_native()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `MyRow` implements `Row`
    /// let row = client.query_one::<MyRow>("SELECT name FROM users WHERE id = 1", None)
    ///     .await
    ///     .unwrap();
    /// if let Some(row) = row {
    ///     println!("Found row: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_one",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = NativeFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_one<T: Row + Send + 'static>(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<Option<T>> {
        self.query_one_params(query, None, qid).await
    }

    /// Executes a `ClickHouse` query with parameters and returns the first row, discarding the
    /// rest.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT name FROM users WHERE id = 1"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<T>`, where `T` is the deserialized row, or
    /// `None` if no rows are returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if row deserialization fails (e.g., schema mismatch).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_native()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `MyRow` implements `Row`
    /// let params = Some(vec![
    ///     ("str", ParamValue::from("name")),
    /// ].into());
    /// let query = "SELECT {str:String} FROM users WHERE id = 1";
    /// let row = client.query_one_params::<MyRow>(query, params, None)
    ///     .await
    ///     .unwrap();
    /// if let Some(row) = row {
    ///     println!("Found row: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_one_params",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = NativeFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_one_params<T: Row + Send + 'static>(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<Option<T>> {
        let mut stream = self.query_params::<T>(query, params, qid).await?;
        stream.next().await.transpose()
    }

    /// Query `ClickHouse` and return rows as dynamic JSON objects.
    ///
    /// This method executes a query and returns the results as a vector of
    /// `serde_json::Map<String, serde_json::Value>` objects, providing schema-less
    /// access to query results. Each row becomes a JSON object with column names
    /// as keys and column values converted to appropriate JSON types.
    ///
    /// This method is the inverse of the serde Object-based dynamic insert functionality,
    /// allowing for flexible data retrieval without predefined structs.
    ///
    /// # Arguments
    /// * `query` - The SQL query to execute
    /// * `qid` - Optional query ID for tracking and debugging
    ///
    /// # Returns
    /// A `Result` containing a vector of JSON objects (one per row)
    ///
    /// # Errors
    /// - Fails if the query is malformed or contains syntax errors.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception.
    /// - Fails if data conversion to JSON fails.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .destination("localhost:9000")
    ///     .build_native()
    ///     .await?;
    ///
    /// let rows = client.query_json("SELECT id, name, created_at FROM users LIMIT 10", None).await?;
    /// for row in rows {
    ///     println!("ID: {}, Name: {}", row["id"], row["name"]);
    /// }
    /// ```
    #[cfg(feature = "serde")]
    #[instrument(
        name = "clickhouse.query_json",
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", db.format = NativeFormat::FORMAT)
    )]
    pub async fn query_json(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        self.query_json_params(query, None, qid).await
    }

    /// Query `ClickHouse` with parameters and return rows as dynamic JSON objects.
    ///
    /// This is the parameterized version of `query_json`.
    #[cfg(feature = "serde")]
    #[instrument(
        name = "clickhouse.query_json_params", 
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", db.format = NativeFormat::FORMAT)
    )]
    pub async fn query_json_params(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        use futures_util::StreamExt;

        let (query, qid) = record_query(qid, query.into(), self.client_id);
        let raw = self.query_raw(query, params, qid).await?;

        let mut all_rows = Vec::new();
        tokio::pin!(raw);

        while let Some(block) = raw.next().await.transpose()? {
            // Convert block to JSON rows
            let json_rows = block_to_json_rows(block)?;
            all_rows.extend(json_rows);
        }

        Ok(all_rows)
    }

    /// Creates a `ClickHouse` table from a Rust struct that implements the `Row` trait.
    ///
    /// This method generates and executes a `CREATE TABLE` DDL statement based on the
    /// structure of the provided `Row` type. The table schema is automatically derived
    /// from the struct fields and their types.
    ///
    /// # Arguments
    /// * `database` - Optional database name. If None, uses the client's default database
    /// * `table` - The name of the table to create
    /// * `options` - Table creation options including engine type, order by, and partition by
    /// * `query_id` - Optional query ID for tracking and debugging
    ///
    /// # Type Parameters
    /// * `T` - A type that implements the `Row` trait, typically a struct with the `#[derive(Row)]`
    ///   macro
    ///
    /// # Example
    /// ```ignore
    /// #[derive(Row)]
    /// struct User {
    ///     id: u32,
    ///     name: String,
    ///     created_at: DateTime,
    /// }
    ///
    /// let options = CreateOptions::new("MergeTree")
    ///     .with_order_by(&["id"]);
    ///
    /// client.create_table::<User>(Some("analytics"), "users", &options, None).await?;
    /// ```
    ///
    /// # Errors
    /// - Returns an error if the table creation fails
    /// - Returns an error if the database/table names are invalid
    /// - Returns an error if the connection is lost
    #[instrument(
        name = "clickhouse.create_table",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "create.table",
            db.format = NativeFormat::FORMAT,
        )
    )]
    pub async fn create_table<T: Row>(
        &self,
        database: Option<&str>,
        table: &str,
        options: &CreateOptions,
        qid: Option<Qid>,
    ) -> Result<()> {
        let database = database.unwrap_or(self.connection.database());
        let stmt = create_table_statement_from_native::<T>(Some(database), table, options)?;
        self.execute(stmt, qid).await?;
        Ok(())
    }

    /// Inserts data by first awaiting the server's header, then building the block from it.
    ///
    /// Use this when the outgoing block must be constructed based on the exact server schema.
    pub async fn insert_with_block_builder(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
        build: impl FnOnce(&[(String, Type)]) -> Result<Block>,
    ) -> Result<impl Stream<Item = Result<()>> + '_> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);

        let (tx_query, rx_query) = oneshot::channel();
        let (header_tx, header_rx) = oneshot::channel();
        let connection = self.conn().await?;

        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    params: None,
                    response: tx_query,
                    header: Some(header_tx),
                },
                qid,
                false,
            )
            .await?;

        let header = header_rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive header for query {qid}")))?;
        let block = build(&header)?;

        let (tx_insert, rx_insert) = oneshot::channel();
        let _ = connection
            .send_operation(Operation::Insert { data: block, response: tx_insert }, qid, true)
            .await?;
        rx_insert.await.map_err(|_| {
            Error::Protocol(format!("Failed to receive response from insert {qid}"))
        })??;

        let responses = rx_query
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;

        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<Block>::weight_insert());

        Ok(self.insert_response(responses, qid))
    }

    /// Begins a streaming insert operation that can accept multiple batches of Serde rows.
    ///
    /// This handle caches the table schema and opens/closes an insert for each flushed batch.
    /// Call `write_rows` one or more times to send batches; call `finish` to ensure the last
    /// partial batch is flushed. For the first iteration, this implementation focuses on tables
    /// with a single JSON column: each Serde row is serialized as the JSON value for that column.
    /// The JSON serializer handles typed paths, defaults for non-nullable typed paths, and
    /// dynamic paths automatically based on the table's JSON type.
    pub async fn insert_into(&self, table: &str, options: InsertOptions) -> Result<InsertInto<'_>> {
        let (database, table) = split_db_table(self.connection.database(), table);
        Ok(InsertInto {
            client: self,
            database: database.to_string(),
            table: table.to_string(),
            options,
            pending: Vec::new(),
            total_rows: 0,
        })
    }
}

fn split_db_table<'a>(default_db: &'a str, table: &'a str) -> (&'a str, &'a str) {
    match table.split_once('.') {
        Some((db, t)) if !db.is_empty() && !t.is_empty() => (db, t),
        _ => {
            let db = if default_db.is_empty() { "default" } else { default_db };
            (db, table)
        }
    }
}

#[derive(Debug, Clone)]
pub struct InsertOptions {
    pub batch_size:          usize,
    pub strict:              bool,
    pub on_missing_default:  bool,
    pub set_object_settings: bool,
    /// Optional explicit column list to include in the INSERT statement,
    /// e.g. INSERT INTO db.table (col1, col2) VALUES ...
    /// When set, the server applies defaults for unspecified columns.
    pub columns:             Option<Vec<String>>,
}

impl InsertOptions {
    pub fn default_batch() -> usize { 10_000 }
}

impl Default for InsertOptions {
    fn default() -> Self {
        Self {
            batch_size:          Self::default_batch(),
            strict:              false,
            on_missing_default:  true,
            set_object_settings: true,
            columns:             None,
        }
    }
}

pub struct InsertInto<'a> {
    client:     &'a Client<NativeFormat>,
    database:   String,
    table:      String,
    options:    InsertOptions,
    pending:    Vec<serde_json::Value>,
    total_rows: usize,
}

impl InsertInto<'_> {
    /// Write a batch of Serde rows. For single JSON-column tables, each row is serialized
    /// as the JSON value for that column.
    pub async fn write_rows<T: serde::Serialize>(
        &mut self,
        rows: impl IntoIterator<Item = T>,
    ) -> Result<usize> {
        // Apply settings once if requested
        if self.options.set_object_settings {
            // Best-effort: ignore errors if already set
            let _unused = self.client.execute("SET allow_experimental_object_type = 1", None).await;
            let _unused =
                self.client.execute("SET allow_suspicious_low_cardinality_types = 1", None).await;
        }

        let mut wrote = 0usize;
        for row in rows {
            let v = serde_json::to_value(row)
                .map_err(|e| Error::SerializeError(format!("serde serialize error: {e}")))?;
            self.pending.push(v);
            if self.pending.len() >= self.options.batch_size {
                self.flush().await?;
            }
            wrote += 1;
        }
        Ok(wrote)
    }

    /// Flush the current batch to the server as a single block.
    pub async fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let rows_len = self.pending.len();

        // Build query and send using header-driven builder
        let query = if let Some(columns) = &self.options.columns {
            // Build explicit column list; backtick-escape identifiers minimally
            let collist = columns
                .iter()
                .map(|c| format!("`{}`", c.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO `{}`.`{}` ({}) VALUES", self.database, self.table, collist)
        } else {
            format!("INSERT INTO `{}`.`{}` VALUES", self.database, self.table)
        };
        tracing::debug!(query = %query, rows = rows_len, "insert_into: executing insert (header-driven)");
        let pending = std::mem::take(&mut self.pending);
        let strict = self.options.strict;
        let on_missing_default = self.options.on_missing_default;
        let mut stream = self
            .client
            .insert_with_block_builder(query, None, move |header: &[(String, Type)]| {
                let column_types: Vec<(String, Type)> = header.to_vec();
                let mut column_data: Vec<Value> = Vec::with_capacity(header.len() * rows_len);
                // Map each column using server-declared names/types
                for (col_name, col_type) in header {
                    let mut values: Vec<Value> = Vec::with_capacity(rows_len);
                    for row in &pending {
                        let cell = row.get(col_name);
                        let val = map_cell_to_value(cell, col_type, strict, on_missing_default)?;
                        values.push(val);
                    }
                    column_data.extend(values.into_iter());
                }
                Ok(Block {
                    info: BlockInfo::default(),
                    rows: rows_len as u64,
                    column_types,
                    column_data,
                })
            })
            .await?;
        while let Some(res) = stream.next().await {
            res?;
        }
        self.total_rows += rows_len;
        // Clear pending rows after successful flush
        self.pending.clear();
        Ok(())
    }

    /// Finish the insert operation by flushing any remaining rows.
    pub async fn finish(mut self) -> Result<()> { self.flush().await }
}

// fetch_table_columns removed: header-driven serialization is now the source of truth
// escape_ident removed

fn map_cell_to_value(
    cell: Option<&serde_json::Value>,
    ty: &Type,
    strict: bool,
    on_missing_default: bool,
) -> Result<Value> {
    match (cell, ty) {
        // Missing cell
        (None, t) => {
            if t.is_nullable() {
                Ok(Value::Null)
            } else if on_missing_default {
                Ok(t.default_value())
            } else {
                Err(Error::SerializeError(format!("missing non-nullable column for type {t:?}")))
            }
        }
        // Present cell, handle JSON/Object specially
        (Some(v), Type::JSON { .. }) => {
            if v.is_null() {
                return Ok(Value::Null);
            }
            // Accept either JSON object/array/value or a string containing JSON
            if let serde_json::Value::String(s) = v {
                // Keep string inputs as strings (back-compat)
                let bytes = match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(parsed) => serde_json::to_vec(&parsed)
                        .map_err(|e| Error::SerializeError(e.to_string()))?,
                    Err(_) => serde_json::to_vec(v)
                        .map_err(|e| Error::SerializeError(e.to_string()))?,
                };
                Ok(Value::String(bytes))
            } else {
                #[cfg(feature = "serde")]
                {
                    Ok(Value::Json(v.clone()))
                }
                #[cfg(not(feature = "serde"))]
                {
                    let bytes = serde_json::to_vec(v)
                        .map_err(|e| Error::SerializeError(e.to_string()))?;
                    Ok(Value::Object(bytes))
                }
            }
        }
        (Some(v), Type::Object) => {
            if v.is_null() {
                return Ok(Value::Null);
            }
            let bytes = if let serde_json::Value::String(s) = v {
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(parsed) => serde_json::to_vec(&parsed)
                        .map_err(|e| Error::SerializeError(e.to_string()))?,
                    Err(_) => {
                        serde_json::to_vec(v).map_err(|e| Error::SerializeError(e.to_string()))?
                    }
                }
            } else {
                serde_json::to_vec(v).map_err(|e| Error::SerializeError(e.to_string()))?
            };
            Ok(Value::Object(bytes))
        }
        // Nullable: recurse into inner type
        (Some(serde_json::Value::Null), t) => {
            if t.is_nullable() {
                Ok(Value::Null)
            } else {
                Ok(t.default_value())
            }
        }
        (Some(v), Type::Nullable(inner) | Type::LowCardinality(inner)) => {
            map_cell_to_value(Some(v), inner, strict, on_missing_default)
        }
        // Scalars
        (Some(v), Type::String | Type::FixedSizedString(_)) => match v {
            serde_json::Value::String(s) => Ok(Value::String(s.as_bytes().to_vec())),
            _ => Ok(Value::String(v.to_string().into_bytes())),
        },
        (Some(v), Type::UInt8) => to_u64(v, strict).and_then(u8_from_u64).map(Value::UInt8),
        (Some(v), Type::UInt16) => to_u64(v, strict).and_then(u16_from_u64).map(Value::UInt16),
        (Some(v), Type::UInt32) => to_u64(v, strict).and_then(u32_from_u64).map(Value::UInt32),
        (Some(v), Type::UInt64) => to_u64(v, strict).map(Value::UInt64),
        (Some(v), Type::Int8) => to_i64(v, strict).and_then(i8_from_i64).map(Value::Int8),
        (Some(v), Type::Int16) => to_i64(v, strict).and_then(i16_from_i64).map(Value::Int16),
        (Some(v), Type::Int32) => to_i64(v, strict).and_then(i32_from_i64).map(Value::Int32),
        (Some(v), Type::Int64) => to_i64(v, strict).map(Value::Int64),
        (Some(v), Type::Float32) => to_f64(v, strict).map(|x| Value::Float32(x as f32)),
        (Some(v), Type::Float64) => to_f64(v, strict).map(Value::Float64),
        // Fallback for unsupported complex types in MVP
        (_, t) => Err(Error::SerializeError(format!("unsupported type mapping for {t:?}"))),
    }
}

fn to_u64(v: &serde_json::Value, strict: bool) -> Result<u64> {
    if let Some(u) = v.as_u64() {
        return Ok(u);
    }
    if !strict {
        if let Some(i) = v.as_i64() {
            return Ok(i as u64);
        }
        if let Some(f) = v.as_f64() {
            return Ok(f as u64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<u64>().map_err(|e| Error::SerializeError(format!("parse u64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to u64".to_string()))
}

fn to_i64(v: &serde_json::Value, strict: bool) -> Result<i64> {
    if let Some(i) = v.as_i64() {
        return Ok(i);
    }
    if !strict {
        if let Some(u) = v.as_u64() {
            return Ok(u as i64);
        }
        if let Some(f) = v.as_f64() {
            return Ok(f as i64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<i64>().map_err(|e| Error::SerializeError(format!("parse i64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to i64".to_string()))
}

fn to_f64(v: &serde_json::Value, strict: bool) -> Result<f64> {
    if let Some(f) = v.as_f64() {
        return Ok(f);
    }
    if !strict {
        if let Some(i) = v.as_i64() {
            return Ok(i as f64);
        }
        if let Some(u) = v.as_u64() {
            return Ok(u as f64);
        }
        if let Some(s) = v.as_str() {
            return s.parse::<f64>().map_err(|e| Error::SerializeError(format!("parse f64: {e}")));
        }
    }
    Err(Error::SerializeError("cannot coerce to f64".to_string()))
}

fn u8_from_u64(x: u64) -> Result<u8> {
    if x <= u8::MAX as u64 {
        Ok(x as u8)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt8")))
    }
}

fn u16_from_u64(x: u64) -> Result<u16> {
    if x <= u16::MAX as u64 {
        Ok(x as u16)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt16")))
    }
}

fn u32_from_u64(x: u64) -> Result<u32> {
    if x <= u32::MAX as u64 {
        Ok(x as u32)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows UInt32")))
    }
}

fn i8_from_i64(x: i64) -> Result<i8> {
    if x >= i8::MIN as i64 && x <= i8::MAX as i64 {
        Ok(x as i8)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int8")))
    }
}

fn i16_from_i64(x: i64) -> Result<i16> {
    if x >= i16::MIN as i64 && x <= i16::MAX as i64 {
        Ok(x as i16)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int16")))
    }
}

fn i32_from_i64(x: i64) -> Result<i32> {
    if x >= i32::MIN as i64 && x <= i32::MAX as i64 {
        Ok(x as i32)
    } else {
        Err(Error::SerializeError(format!("value {x} overflows Int32")))
    }
}

impl Client<ArrowFormat> {
    /// Executes a `ClickHouse` query and streams Arrow [`RecordBatch`] results.
    ///
    /// This method sends a query to `ClickHouse` and returns a stream of [`RecordBatch`]
    /// instances, each containing a chunk of the query results in Apache Arrow format.
    /// Use this method for efficient integration with Arrow-based data processing
    /// pipelines. For row-based access, consider [`Client::query_rows`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<RecordBatch>`] that streams
    /// query results.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let mut response = client.query("SELECT * FROM my_table", None).await.unwrap();
    /// while let Some(batch) = response.next().await {
    ///     let batch = batch.unwrap();
    ///     println!("Received batch with {} rows", batch.num_rows());
    /// }
    /// ```
    #[instrument(
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", clickhouse.query.id)
    )]
    pub async fn query(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<RecordBatch>> {
        self.query_params(query, None, qid).await
    }

    /// Executes a `ClickHouse` query with parameters and streams Arrow [`RecordBatch`] results.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<RecordBatch>`] that streams
    /// query results.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let params = Some(vec![("name", ParamValue::from("my_table"))].into());
    /// let query = "SELECT * FROM {name:Identifier}";
    /// let mut response = client.query_params(query, params, None).await.unwrap();
    /// while let Some(batch) = response.next().await {
    ///     let batch = batch.unwrap();
    ///     println!("Received batch with {} rows", batch.num_rows());
    /// }
    /// ```
    #[instrument(
        skip_all,
        fields(db.system = "clickhouse", db.operation = "query", clickhouse.query.id)
    )]
    pub async fn query_params(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<RecordBatch>> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);
        Ok(ClickHouseResponse::new(Box::pin(self.query_raw(query, params, qid).await?)))
    }

    /// Executes a `ClickHouse` query and streams rows as column-major values.
    ///
    /// This method sends a query to `ClickHouse` and returns a stream of rows, where
    /// each row is represented as a `Vec<Value>` containing column values. The data is
    /// transposed from Arrow [`RecordBatch`] format to row-major format, making it
    /// convenient for row-based processing. For direct Arrow access, use
    /// [`Client::query`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM my_table"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a [`ClickHouseResponse<Vec<Value>>`] that streams rows.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let mut response = client.query_rows("SELECT * FROM my_table", None).await.unwrap();
    /// while let Some(row) = response.next().await {
    ///     let row = row.unwrap();
    ///     println!("Row values: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_rows",
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        ),
        skip_all
    )]
    pub async fn query_rows(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<ClickHouseResponse<Vec<Value>>> {
        let (query, qid) = record_query(qid, query.into(), self.client_id);
        let connection = self.conn().await?;

        // Create metadata channel
        let (tx, rx) = oneshot::channel();
        let (header_tx, header_rx) = oneshot::channel();

        #[cfg_attr(not(feature = "inner_pool"), expect(unused_variables))]
        let conn_idx = connection
            .send_operation(
                Operation::Query {
                    query,
                    settings: self.settings.clone(),
                    // TODO: Add arg for params
                    params: None,
                    response: tx,
                    header: Some(header_tx),
                },
                qid,
                true,
            )
            .await?;

        trace!({ ATT_CID } = self.client_id, { ATT_QID } = %qid, "sent query, awaiting response");
        let responses = rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive response for query {qid}")))?
            .inspect_err(|error| error!(?error, { ATT_QID } = %qid, "Error receiving header"))?;

        let header = header_rx
            .await
            .map_err(|_| Error::Protocol(format!("Failed to receive header for query {qid}")))?;

        let response = create_response_stream::<ArrowFormat>(responses, qid, self.client_id)
            .map(move |batch| (header.clone(), batch))
            .map(|(header, batch)| {
                let batch = batch?;
                let batch_iter = batch_to_rows(&batch, Some(&header))?;
                Ok::<_, Error>(stream::iter(batch_iter))
            })
            .try_flatten();

        // Decrement load balancer
        #[cfg(feature = "inner_pool")]
        connection.finish(conn_idx, Operation::<RecordBatch>::weight_insert_many());

        Ok(ClickHouseResponse::from_stream(response))
    }

    /// Executes a `ClickHouse` query and returns the first column of the first batch.
    ///
    /// This method sends a query to `ClickHouse` and returns the first column of the
    /// first [`RecordBatch`] as an Arrow [`ArrayRef`], or `None` if the result is empty.
    /// It is useful for queries that return a single column (e.g., `SELECT id FROM
    /// my_table`). For full batch access, use [`Client::query`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT id FROM my_table"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<ArrayRef>`, representing the first column of
    /// the first batch, or `None` if no data is returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let column = client.query_column("SELECT id FROM my_table", None)
    ///     .await
    ///     .unwrap();
    /// if let Some(col) = column {
    ///     println!("Column data: {:?}", col);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_column",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_column(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<Option<ArrayRef>> {
        self.query_column_params(query, None, qid).await
    }

    /// Executes a `ClickHouse` query with parameters and returns the first column of the first
    /// batch.
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT id FROM my_table"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<ArrayRef>`, representing the first column of
    /// the first batch, or `None` if no data is returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let params = Some(vec![("name", ParamValue::from("my_table"))].into());
    /// let query = "SELECT id FROM {name:Identifier}";
    /// let column = client.query_column_params("SELECT id FROM my_table", params, None)
    ///     .await
    ///     .unwrap();
    /// if let Some(col) = column {
    ///     println!("Column data: {:?}", col);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_column_params",
        skip_all,
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_column_params(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<Option<ArrayRef>> {
        let mut stream = self.query_params(query, params, qid).await?;
        let Some(batch) = stream.next().await.transpose()? else {
            return Ok(None);
        };

        if batch.num_rows() == 0 { Ok(None) } else { Ok(Some(Arc::clone(batch.column(0)))) }
    }

    /// Executes a `ClickHouse` query and returns the first row as a [`RecordBatch`].
    ///
    /// This method sends a query to `ClickHouse` and returns the first row of the first
    /// [`RecordBatch`], or `None` if the result is empty. The returned [`RecordBatch`]
    /// contains a single row. It is useful for queries expected to return a single row
    /// (e.g., `SELECT * FROM users WHERE id = 1`). For streaming multiple rows, use
    /// [`Client::query`].
    ///
    /// Progress and profile events are dispatched to the client's event channel (see
    /// [`Client::subscribe_events`]).
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM users WHERE id = 1"`).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<RecordBatch>`, representing the first row, or
    /// `None` if no rows are returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let batch = client.query_one("SELECT * FROM users WHERE id = 1", None)
    ///     .await
    ///     .unwrap();
    /// if let Some(row) = batch {
    ///     println!("Row data: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_one",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_one(
        &self,
        query: impl Into<ParsedQuery>,
        qid: Option<Qid>,
    ) -> Result<Option<RecordBatch>> {
        self.query_one_params(query, None, qid).await
    }

    /// Executes a `ClickHouse` query with parameters and returns the first row as a
    /// [`RecordBatch`].
    ///
    /// # Parameters
    /// - `query`: The SQL query to execute (e.g., `"SELECT * FROM users WHERE id = 1"`).
    /// - `params`: The query parameters to provide
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing an `Option<RecordBatch>`, representing the first row, or
    /// `None` if no rows are returned.
    ///
    /// # Errors
    /// - Fails if the query is malformed or unsupported by `ClickHouse`.
    /// - Fails if the connection to `ClickHouse` is interrupted.
    /// - Fails if `ClickHouse` returns an exception (e.g., table not found).
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let params = Some(vec![("id", ParamValue::from(1))]);
    /// let batch = client.query_one_params("SELECT * FROM users WHERE id = {id:UInt64}", None)
    ///     .await
    ///     .unwrap();
    /// if let Some(row) = batch {
    ///     println!("Row data: {:?}", row);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.query_one_params",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn query_one_params(
        &self,
        query: impl Into<ParsedQuery>,
        params: Option<QueryParams>,
        qid: Option<Qid>,
    ) -> Result<Option<RecordBatch>> {
        let stream = self.query_params(query, params, qid).await?;
        tokio::pin!(stream);

        let Some(batch) = stream.next().await.transpose()? else {
            return Ok(None);
        };

        if batch.num_rows() == 0 {
            Ok(None)
        } else {
            Ok(Some(take_record_batch(&batch, &arrow::array::UInt32Array::from(vec![0]))?))
        }
    }

    /// Fetches the list of database names (schemas) in `ClickHouse`.
    ///
    /// This method queries `ClickHouse` to retrieve the names of all databases
    /// accessible to the client. It is useful for exploring the database structure or
    /// validating database existence before performing operations.
    ///
    /// # Parameters
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a `Vec<String>` of database names.
    ///
    /// # Errors
    /// - Fails if the query execution encounters a `ClickHouse` error (e.g., permission denied).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let schemas = client.fetch_schemas(None).await.unwrap();
    /// println!("Databases: {:?}", schemas);
    /// ```
    #[instrument(
        name = "clickhouse.fetch_schemas",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn fetch_schemas(&self, qid: Option<Qid>) -> Result<Vec<String>> {
        crate::arrow::schema::fetch_databases(self, qid).await
    }

    /// Fetches all tables across all databases in `ClickHouse`.
    ///
    /// This method queries `ClickHouse` to retrieve a mapping of database names to
    /// their table names. It is useful for discovering the full schema structure of
    /// the `ClickHouse` instance.
    ///
    /// # Parameters
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a `HashMap<String, Vec<String>>`, where each key is a
    /// database name and the value is a list of table names in that database.
    ///
    /// # Errors
    /// - Fails if the query execution encounters a `ClickHouse` error (e.g., permission denied).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let tables = client.fetch_all_tables(None).await.unwrap();
    /// for (db, tables) in tables {
    ///     println!("Database {} has tables: {:?}", db, tables);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.fetch_all_tables",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn fetch_all_tables(&self, qid: Option<Qid>) -> Result<HashMap<String, Vec<String>>> {
        crate::arrow::schema::fetch_all_tables(self, qid).await
    }

    /// Fetches the list of table names in a specific `ClickHouse` database.
    ///
    /// This method queries `ClickHouse` to retrieve the names of all tables in the
    /// specified database (or the client's default database if `None`). It is useful
    /// for exploring the schema of a specific database.
    ///
    /// # Parameters
    /// - `database`: Optional database name. If `None`, uses the client's default database.
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a `Vec<String>` of table names.
    ///
    /// # Errors
    /// - Fails if the database does not exist or is inaccessible.
    /// - Fails if the query execution encounters a `ClickHouse` error (e.g., permission denied).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let tables = client.fetch_tables(Some("my_db"), None).await.unwrap();
    /// println!("Tables in my_db: {:?}", tables);
    /// ```
    #[instrument(
        name = "clickhouse.fetch_tables",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn fetch_tables(
        &self,
        database: Option<&str>,
        qid: Option<Qid>,
    ) -> Result<Vec<String>> {
        let database = database.unwrap_or(self.connection.database());
        crate::arrow::schema::fetch_tables(self, database, qid).await
    }

    /// Fetches the schema of specified tables in a `ClickHouse` database.
    ///
    /// This method queries `ClickHouse` to retrieve the Arrow schemas of the specified
    /// tables in the given database (or the client's default database if `None`). If
    /// the `tables` list is empty, it fetches schemas for all tables in the database.
    /// The result is a mapping of table names to their corresponding Arrow [`SchemaRef`].
    ///
    /// # Parameters
    /// - `database`: Optional database name. If `None`, uses the client's default database.
    /// - `tables`: A list of table names to fetch schemas for. An empty list fetches all tables.
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] containing a `HashMap<String, SchemaRef>`, mapping table names to
    /// their schemas.
    ///
    /// # Errors
    /// - Fails if the database or any table does not exist or is inaccessible.
    /// - Fails if the query execution encounters a `ClickHouse` error (e.g., permission denied).
    /// - Fails if the connection to `ClickHouse` is interrupted.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// let schemas = client.fetch_schema(Some("my_db"), &["my_table"], None)
    ///     .await
    ///     .unwrap();
    /// for (table, schema) in schemas {
    ///     println!("Table {} schema: {:?}", table, schema);
    /// }
    /// ```
    #[instrument(
        name = "clickhouse.fetch_schema",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "query",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn fetch_schema(
        &self,
        database: Option<&str>,
        tables: &[&str],
        qid: Option<Qid>,
    ) -> Result<HashMap<String, SchemaRef>> {
        let database = database.unwrap_or(self.connection.database());
        let options = self.connection.metadata().arrow_options;
        crate::arrow::schema::fetch_schema(self, database, tables, qid, options).await
    }

    /// Issues a `CREATE TABLE` DDL statement for a table using Arrow schema.
    ///
    /// Creates a table in the specified database (or the client's default database if
    /// `None`) based on the provided Arrow [`SchemaRef`]. The `options` parameter allows
    /// customization of table properties, such as engine type and partitioning. This
    /// method is specific to [`ArrowClient`] for seamless integration with Arrow-based
    /// data pipelines.
    ///
    /// # Parameters
    /// - `database`: Optional database name. If `None`, uses the client's default database.
    /// - `table`: Name of the table to create.
    /// - `schema`: The Arrow schema defining the table's structure.
    /// - `options`: Configuration for table creation (e.g., engine, partitioning).
    /// - `qid`: Optional query ID for tracking and debugging.
    ///
    /// # Returns
    /// A [`Result`] indicating success or failure of the operation.
    ///
    /// # Errors
    /// - Fails if the provided schema is invalid or incompatible with `ClickHouse`.
    /// - Fails if the database does not exist or is inaccessible.
    /// - Fails if the query execution encounters a `ClickHouse` error.
    ///
    /// # Examples
    /// ```rust,ignore
    /// use clickhouse_arrow::prelude::*;
    /// use arrow::datatypes::{Schema, SchemaRef};
    ///
    /// let client = Client::builder()
    ///     .with_endpoint("localhost:9000")
    ///     .build_arrow()
    ///     .await
    ///     .unwrap();
    ///
    /// // Assume `schema` is a valid Arrow schema
    /// let schema: SchemaRef = Arc::new(Schema::new(vec![/* ... */]));
    /// let options = CreateOptions::default();
    /// client.create_table(Some("my_db"), "my_table", &schema, &options, None)
    ///     .await
    ///     .unwrap();
    /// ```
    #[instrument(
        name = "clickhouse.create_table",
        skip_all
        fields(
            db.system = "clickhouse",
            db.operation = "create.table",
            db.format = ArrowFormat::FORMAT,
            clickhouse.client.id = self.client_id,
            clickhouse.query.id
        )
    )]
    pub async fn create_table(
        &self,
        database: Option<&str>,
        table: &str,
        schema: &SchemaRef,
        options: &CreateOptions,
        qid: Option<Qid>,
    ) -> Result<()> {
        let database = database.unwrap_or(self.connection.database());
        let arrow_options = self.connection.metadata().arrow_options;
        let stmt = create_table_statement_from_arrow(
            Some(database),
            table,
            schema,
            options,
            Some(arrow_options),
        )?;
        self.execute(stmt, qid).await?;
        Ok(())
    }
}

impl<T: ClientFormat> Drop for Client<T> {
    fn drop(&mut self) {
        trace!({ ATT_CID } = self.client_id, "Client dropped");
    }
}

/// Simple helper to log query id and client id
fn record_query(qid: Option<Qid>, query: ParsedQuery, cid: u16) -> (String, Qid) {
    let qid = qid.unwrap_or_default();
    let _ = Span::current().record(ATT_QID, tracing::field::display(qid));
    let query = query.0;
    trace!(query, { ATT_CID } = cid, "Querying clickhouse");
    (query, qid)
}

/// Convert a Block to a vector of JSON objects (`serde_json::Map`).
/// Each row becomes a JSON object with column names as keys.
#[cfg(feature = "serde")]
fn block_to_json_rows(mut block: Block) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut json_rows = Vec::with_capacity(block.rows as usize);

    // Use the block's row iterator to get row data
    let mut row_iter = block.take_iter_rows();

    while let Some(row_data) = row_iter.next() {
        let mut json_object = serde_json::Map::new();

        for (column_name, _column_type, value) in row_data {
            let json_value = value.to_json().map_err(|e| Error::DeserializeError(e.to_string()))?;
            let _previous = json_object.insert(column_name.to_string(), json_value);
        }

        json_rows.push(json_object);
    }

    Ok(json_rows)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::Int32Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    use super::*;

    #[test]
    fn test_record_query_function() {
        let query = ParsedQuery("SELECT 1".to_string());
        let cid = 123;
        let qid = Qid::new();

        let (parsed_query, returned_qid) = record_query(Some(qid), query, cid);

        assert_eq!(parsed_query, "SELECT 1");
        assert_eq!(returned_qid, qid);
    }

    #[test]
    fn test_record_query_function_no_qid() {
        let query = ParsedQuery("SELECT 2".to_string());
        let cid = 456;

        let (parsed_query, returned_qid) = record_query(None, query, cid);

        assert_eq!(parsed_query, "SELECT 2");
        // Should generate a default Qid
        assert!(!returned_qid.to_string().is_empty());
    }

    // Helper function to create a simple test RecordBatch
    fn create_test_record_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
        let array = Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]));
        RecordBatch::try_new(schema, vec![array]).unwrap()
    }

    #[tokio::test]
    async fn test_split_record_batch_function() {
        // Test the split_record_batch function that insert_max_rows uses
        let batch = create_test_record_batch();
        let max_rows = 2;

        let batches = crate::arrow::utils::split_record_batch(batch, max_rows);

        // Should split 5 rows into 3 batches: [2, 2, 1]
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(batches[1].num_rows(), 2);
        assert_eq!(batches[2].num_rows(), 1);
    }

    #[test]
    fn test_split_record_batch_edge_cases() {
        let batch = create_test_record_batch();

        // Test with max_rows equal to batch size
        let batches = crate::arrow::utils::split_record_batch(batch.clone(), 5);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 5);

        // Test with max_rows larger than batch size
        let batches = crate::arrow::utils::split_record_batch(batch.clone(), 10);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 5);

        // Test with max_rows = 1
        let batches = crate::arrow::utils::split_record_batch(batch, 1);
        assert_eq!(batches.len(), 5);
        for batch in batches {
            assert_eq!(batch.num_rows(), 1);
        }
    }
}
