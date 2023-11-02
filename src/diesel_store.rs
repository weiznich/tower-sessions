//! A session store backed by a diesel connection pool
use std::marker::PhantomData;

use async_trait::async_trait;
use diesel::{
    expression_methods::ExpressionMethods,
    prelude::{BoolExpressionMethods, OptionalExtension, QueryDsl, RunQueryDsl},
};

use crate::{session_store::ExpiredDeletion, SessionStore};

/// An error type for diesel stores
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum DieselStoreError {
    /// A pool related error
    #[cfg(feature = "diesel-r2d2")]
    #[error("Pool Error: {0}")]
    R2D2Error(#[from] diesel::r2d2::PoolError),
    /// A diesel related error
    #[error("Diesel Error: {0}")]
    DieselError(#[from] diesel::result::Error),
    /// Failed to join a blocking tokio task
    #[cfg(feature = "diesel-r2d2")]
    #[error("Failed to join task: {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// A variant to map `rmp_serde` encode errors.
    #[error("Failed to serialize session data: {0}")]
    SerializationError(#[from] rmp_serde::encode::Error),
    /// A variant to map `rmp_serde` encode errors.
    #[error("Failed to deserialize session data: {0}")]
    DeserializationError(#[from] rmp_serde::decode::Error),
    #[cfg(feature = "diesel-deadpool")]
    #[error("Failed to interact with deadpool: {0}")]
    /// A variant that indicates that we cannot interact with deadpool
    InteractError(String),
    #[cfg(feature = "diesel-deadpool")]
    #[error("Failed to get a connection from deadpool: {0}")]
    /// A variant that indicates that we cannot get a connection from deadpool
    DeadpoolError(#[from] deadpool_diesel::PoolError),
}

/// A Diesel session store
#[derive(Debug)]
pub struct DieselStore<P, C> {
    pool: P,
    c: PhantomData<fn(C)>,
}

impl<P: Clone, C> Clone for DieselStore<P, C> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            c: self.c.clone(),
        }
    }
}

diesel::table! {
    /// The session table used by default by the diesel-store implemenattion
    sessions {
        /// `id` column, contains a session id
        id -> Text,
        /// `expiry_date` column, contains a required expiry timestamp
        expiry_date -> Timestamp,
        /// `data` column, contains serialized session data
        data -> Binary,
    }
}

/// A helper trait to abstract over different pooling solutions for diesel
#[async_trait::async_trait]
pub trait DieselPool: Clone + Sync + Send + 'static {
    /// The connection type used by this pool
    type Connection: diesel::Connection;
    /// Interact with a connection from that pool
    async fn interact<R, E, F>(&self, c: F) -> Result<R, DieselStoreError>
    where
        R: Send + 'static,
        E: Send + 'static,
        F: FnOnce(&mut Self::Connection) -> Result<R, E> + Send + 'static,
        DieselStoreError: From<E>;
}

#[cfg(feature = "diesel-r2d2")]
#[async_trait::async_trait]
impl<C> DieselPool<C> for diesel::r2d2::Pool<diesel::r2d2::ConnectionManager<C>> {
    async fn interact<R, E, F>(&self, c: F) -> Result<R, DieselStoreError>
    where
        F: FnOnce(&mut C) -> Result<R, E> + Send + 'static,
        DieselStoreError: From<E>,
        R: Send + 'static,
        E: Send + 'static,
    {
        let pool = self.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            let r = c(&mut *conn)?;
            Ok::<_, DieselStoreError>(r)
        })
        .await??)
    }
}

#[cfg(feature = "diesel-deadpool")]
#[async_trait::async_trait]
impl<C> DieselPool for deadpool_diesel::Pool<deadpool_diesel::Manager<C>>
where
    C: Connection + 'static,
    deadpool_diesel::Manager<C>: deadpool::managed::Manager<Type = deadpool_diesel::Connection<C>>,
    <deadpool_diesel::Manager<C> as deadpool::managed::Manager>::Type: Send + Sync,
    <deadpool_diesel::Manager<C> as deadpool::managed::Manager>::Error: std::fmt::Debug,
    DieselStoreError: From<
        deadpool::managed::PoolError<
            <deadpool_diesel::Manager<C> as deadpool::managed::Manager>::Error,
        >,
    >,
{
    type Connection = C;

    async fn interact<R, E, F>(&self, c: F) -> Result<R, DieselStoreError>
    where
        F: FnOnce(&mut Self::Connection) -> Result<R, E> + Send + 'static,
        DieselStoreError: From<E>,
        R: Send + 'static,
        E: Send + 'static,
    {
        let conn = self.get().await?;
        let r = conn
            .as_ref()
            .interact(c)
            .await
            .map_err(|e| DieselStoreError::InteractError(e.to_string()))?;
        r.map_err(Into::into)
    }
}

impl<P> DieselStore<P, P::Connection>
where
    P: DieselPool,
{
    /// Create a new diesel store with a provided connection pool.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # #[cfg(feature = "diesel-r2d2")]
    /// # fn main() {
    /// use diesel::{
    ///     prelude::*,
    ///     r2d2::{ConnectionManager, Pool},
    /// };
    /// use tower_sessions::diesel_store::DieselStore;
    ///
    /// let pool = Pool::builder()
    ///     .build(ConnectionManager::<SqliteConnection>::new(":memory:"))
    ///     .unwrap();
    /// let session_store = DieselStore::new(pool);
    /// # }
    ///
    /// # #[cfg(not(feature = "diesel-r2d2"))]
    /// # fn main() {}
    /// ```
    pub fn new(pool: P) -> Self {
        Self { pool, c: PhantomData }
    }
}

#[async_trait::async_trait]
impl<P> SessionStore for DieselStore<P, diesel::PgConnection>
where
    P: DieselPool<Connection = diesel::PgConnection>,
{
    type Error = DieselStoreError;

    async fn save(&self, session_record: &crate::Session) -> Result<(), Self::Error> {
        let expiry_date = session_record.expiry_date();
        let expiry_date = time::PrimitiveDateTime::new(expiry_date.date(), expiry_date.time());
        let data = rmp_serde::to_vec(session_record)?;
        let session_id = session_record.id().to_string();
        self.pool
            .interact(move |conn| {
                diesel::insert_into(sessions::table)
                    .values((
                        sessions::id.eq(session_id),
                        sessions::expiry_date.eq(expiry_date),
                        sessions::data.eq(&data),
                    ))
                    .on_conflict(sessions::id)
                    .do_update()
                    .set((
                        sessions::expiry_date.eq(expiry_date),
                        sessions::data.eq(&data),
                    ))
                    .execute(conn)
            })
            .await?;
        Ok(())
    }

    async fn load(
        &self,
        session_id: &crate::session::Id,
    ) -> Result<Option<crate::Session>, Self::Error> {
        let session_id = session_id.to_string();
        let res = self
            .pool
            .interact(move |conn| {
                let q = sessions::table
                    .limit(1)
                    .select(sessions::data)
                    .filter(
                        sessions::id
                            .eq(session_id.to_string())
                            .and(sessions::expiry_date.gt(diesel::dsl::now)),
                    )
                    .get_result::<Vec<u8>>(conn)
                    .optional()?
                    .map(|data| rmp_serde::from_slice(&data))
                    .transpose()?;
                Ok::<_, DieselStoreError>(q)
            })
            .await?;
        Ok(res)
    }

    async fn delete(&self, session_id: &crate::session::Id) -> Result<(), Self::Error> {
        let session_id = session_id.to_string();
        self.pool
            .interact(move |conn| {
                diesel::delete(sessions::table.find(session_id)).execute(conn)?;
                Ok::<_, DieselStoreError>(())
            })
            .await?;
        Ok(())
    }
}

#[async_trait]
impl<P> ExpiredDeletion for DieselStore<P, diesel::PgConnection>
where
    P: DieselPool<Connection = diesel::PgConnection>,
{
    async fn delete_expired(&self) -> Result<(), Self::Error> {
        self.pool
            .interact(|conn| {
                diesel::delete(sessions::table.filter(sessions::expiry_date.lt(diesel::dsl::now)))
                    .execute(conn)?;
                Ok::<_, DieselStoreError>(())
            })
            .await?;
        Ok(())
    }
}
