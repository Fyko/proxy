use super::{make_route, RatelimitInfo, Ratelimiter};
#[cfg(test)]
use anyhow::anyhow;
use anyhow::{Context, Result};
use redis::Script;
use reqwest::{Client, Request, Response};
use std::{convert::Into, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
	stream::StreamExt,
	sync::{broadcast, Mutex},
};

static NOTIFY_KEY: &'static str = "rest_ready";

pub struct RedisRatelimiter {
	redis: Mutex<redis::aio::Connection>,
	http: Client,
	claim_script: Script,
	release_script: Script,
	ready_publisher: broadcast::Sender<String>,
}

impl RedisRatelimiter {
	pub async fn new(redis: &redis::Client) -> Result<Self> {
		let pubsub = redis.get_async_connection().await?.into_pubsub();
		let main = redis.get_async_connection().await?;
		Self::new_from_connections(main, pubsub).await
	}

	pub async fn new_from_connections(
		main: redis::aio::Connection,
		mut pubsub: redis::aio::PubSub,
	) -> Result<Self> {
		pubsub.subscribe(NOTIFY_KEY).await?;
		let (sender, _) = broadcast::channel(32);

		let pubsub_sender = sender.clone();
		tokio::task::spawn(async move {
			while let Some(msg) = pubsub.on_message().next().await {
				let _ = pubsub_sender.send(msg.get_payload().unwrap());
			}
		});

		Ok(Self {
			redis: Mutex::new(main),
			http: Client::new(),
			claim_script: Script::new(include_str!("./scripts/claim.lua")),
			release_script: Script::new(include_str!("./scripts/release.lua")),
			ready_publisher: sender,
		})
	}

	#[cfg(test)]
	async fn claim_timeout(&self, bucket: &str, timeout: Duration) -> Result<()> {
		tokio::select! {
			result = self.claim(bucket) => result,
			_ = tokio::time::delay_for(timeout) => {
				Err(anyhow!("failed to claim \"{}\" in {}s", bucket, timeout.as_secs()))
			}
		}
	}

	async fn claim(&self, bucket: &str) -> Result<()> {
		'outer: loop {
			let expiration: isize = self
				.claim_script
				.key(bucket)
				.key(bucket.to_string() + "_size")
				.invoke_async(&mut *self.redis.lock().await)
				.await?;

			if expiration > 0 {
				println!("known expiration: {}", expiration);
				tokio::time::delay_for(Duration::from_millis(expiration as u64)).await;
				continue;
			}

			if expiration == 0 {
				println!("ready!");
				break;
			}

			println!("waiting for pubsub notification");
			let mut rcv = self.ready_publisher.subscribe();
			loop {
				let opened_bucket = rcv.recv().await?;
				if opened_bucket == bucket {
					println!("bucket open!");
					continue 'outer;
				}
			}
		}

		Ok(())
	}

	async fn release<'a>(&self, bucket: &str, info: impl Into<RatelimitInfo>) -> Result<()> {
		let info = info.into();

		self.release_script
			.key(bucket)
			.key(bucket.to_string() + "_size")
			.key(NOTIFY_KEY)
			.arg(info.limit.unwrap_or(0))
			.arg(info.resets_at.unwrap_or(0) as usize)
			.invoke_async(&mut *self.redis.lock().await)
			.await?;

		Ok(())
	}
}

impl Ratelimiter for RedisRatelimiter {
	fn make_request(
		self: Arc<Self>,
		req: Request,
	) -> Pin<Box<dyn Future<Output = Result<Response>> + Send>> {
		let this = Arc::clone(&self);
		Box::pin(async move {
			let bucket = make_route(req.url().path())?;

			this.claim(&bucket)
				.await
				.with_context(|| format!("Unable to claim bucket \"{}\"", &bucket))?;
			let response = this.http.execute(req).await;
			this.release(&bucket, response.as_ref())
				.await
				.with_context(|| format!("Unable to release bucket \"{}\"", &bucket))?;

			Ok(response?)
		})
	}
}

#[cfg(test)]
mod test {
	use super::{super::RatelimitInfo, RedisRatelimiter};
	use anyhow::{Error, Result};
	use redis::Client;
	use std::{
		sync::{
			atomic::{AtomicBool, AtomicUsize, Ordering},
			Arc,
		},
		time::{Duration, SystemTime, UNIX_EPOCH},
	};

	static NEXT_DB: AtomicUsize = AtomicUsize::new(0);

	async fn get_client() -> Result<RedisRatelimiter> {
		let db = NEXT_DB.fetch_add(1, Ordering::Relaxed);
		dbg!(db);

		let client = Client::open("redis://localhost:6379")?;
		let mut conn = client.get_async_connection().await?;
		let _: redis::Value = redis::cmd("SELECT").arg(db).query_async(&mut conn).await?;
		let _: redis::Value = redis::cmd("FLUSHDB").query_async(&mut conn).await?;

		let pubsub = client.get_async_connection().await?;
		let _: redis::Value = redis::cmd("SELECT").arg(db).query_async(&mut conn).await?;
		let pubsub = pubsub.into_pubsub();

		Ok(RedisRatelimiter::new_from_connections(conn, pubsub).await?)
	}

	#[tokio::test]
	async fn claim_release() -> Result<()> {
		let client = get_client().await?;
		client.claim_timeout("foo", Duration::from_secs(5)).await?;

		let client = Arc::new(client);
		let released = Arc::new(AtomicBool::new(false));
		tokio::try_join!(
			async {
				client.claim_timeout("foo", Duration::from_secs(5)).await?;
				match released.load(Ordering::Relaxed) {
					true => Ok(()),
					false => Err(anyhow::anyhow!("claimed before lock was released")),
				}
			},
			async {
				client.release("foo", RatelimitInfo { limit: None, resets_at: None }).await?;
				released.store(true, Ordering::Relaxed);
				Ok(())
			},
		)?;

		Ok(())
	}

	#[tokio::test]
	async fn claim_timeout_release() -> Result<()> {
		let client = get_client().await?;

		client.claim_timeout("foo", Duration::from_secs(5)).await?;

		let start = SystemTime::now().duration_since(UNIX_EPOCH)?;
		let target = start + Duration::from_secs(5);

		client
			.release(
				"foo",
				RatelimitInfo {
					limit: None,
					resets_at: Some(target.as_millis()),
				},
			)
			.await?;

		client.claim_timeout("foo", Duration::from_secs(1)).await?;
		client.claim_timeout("foo", Duration::from_secs(6)).await?;

		let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
		assert!(
			now > target,
			"claimed lock {}ms after start (expected at least 5s)",
			(now - start).as_millis()
		);

		Ok(())
	}

	#[tokio::test]
	async fn claim_3x() -> Result<()> {
		let client = get_client().await?;
		let client = Arc::new(client);
		let release_client = Arc::clone(&client);
		tokio::try_join!(
			client.claim_timeout("foo", Duration::from_secs(1)),
			client.claim_timeout("foo", Duration::from_secs(6)),
			client.claim_timeout("foo", Duration::from_secs(11)),
			async {
				tokio::spawn(async move {
					release_client
						.release(
							"foo",
							RatelimitInfo {
								limit: None,
								resets_at: None,
							},
						)
						.await?;
					tokio::time::delay_for(Duration::from_secs(5)).await;
					release_client
						.release(
							"foo",
							RatelimitInfo {
								limit: None,
								resets_at: None,
							},
						)
						.await?;
					tokio::time::delay_for(Duration::from_secs(5)).await;
					release_client
						.release(
							"foo",
							RatelimitInfo {
								limit: None,
								resets_at: None,
							},
						)
						.await?;
					Ok::<(), Error>(())
				})
				.await?
			}
		)?;

		Ok(())
	}

	#[tokio::test]
	async fn claim_limit_release() -> Result<()> {
		let client = get_client().await?;

		client.claim_timeout("foo", Duration::from_secs(1)).await?;

		client
			.release(
				"foo",
				RatelimitInfo {
					limit: Some(2),
					resets_at: None,
				},
			)
			.await?;

		for _ in 0..2 {
			client.claim_timeout("foo", Duration::from_secs(1)).await?;
		}

		let released = AtomicBool::new(false);
		tokio::try_join!(
			async {
				client.claim_timeout("foo", Duration::from_secs(6)).await?;
				match released.load(Ordering::Relaxed) {
					true => Ok(()),
					false => Err(anyhow::anyhow!("claimed before release")),
				}
			},
			async {
				tokio::time::delay_for(Duration::from_secs(5)).await;
				client.release("foo", RatelimitInfo {
					limit: Some(2),
					resets_at: None,
				}).await?;
				released.store(true, Ordering::Relaxed);
				Ok(())
			}
		)?;

		Ok(())
	}
}
