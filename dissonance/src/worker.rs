use std::{error::Error, sync::Arc, time::Instant};

use dashmap::DashMap;
use futures::StreamExt;
use tracing::{debug, info, warn};
use twilight_gateway::{cluster::ShardScheme, Cluster, Event, EventTypeFlags, Intents};
use twilight_http::Client;
use twilight_model::{
    channel::Channel,
    gateway::{
        payload::{
            ChannelCreate, ChannelDelete, ChannelUpdate, MemberChunk, RequestGuildMembers,
            UpdateStatus,
        },
        presence::{Activity, ActivityType, Status},
    },
    guild::{Guild, GuildStatus},
    id::GuildId,
};

use crate::{cache::Cache, database::Database, forwarder::Forwarder};

type VoidResult = Result<(), Box<dyn Error>>;

const VERSION: &str = compile_time_run::run_command_str!("git", "log", "--format=%h - %s", "-n 1");

const STATUSES: &[(ActivityType, &'static str)] = &[
    (ActivityType::Playing, "on-hit Orianna"),
    (ActivityType::Playing, "with the Ball"),
    (ActivityType::Listening, "time ticking away"),
    (ActivityType::Watching, "my enemies shiver"),
    (ActivityType::Watching, "you"),
    (ActivityType::Watching, "Piltovan theater"),
    (ActivityType::Listening, "Running in the 90s"),
    (ActivityType::Playing, "with Stopwatch"),
    (ActivityType::Watching, "imaqtpie"),
    (ActivityType::Listening, "them scream"),
    (ActivityType::Watching, "what makes them tick"),
    (ActivityType::Playing, "Command: Attack"),
    (ActivityType::Playing, "Command: Dissonance"),
    (ActivityType::Playing, "Command: Protect"),
    (ActivityType::Playing, "Command: Shockwave"),
];

pub(crate) struct Worker {
    db: Database,
    cluster: Cluster,
    cache: Cache,
    forwarder: Forwarder,
    outstanding_member_requests: DashMap<GuildId, Instant>,
}

macro_rules! handle_event {
    ($event:literal, $body:expr) => {
        tokio::spawn(async move {
            match $body {
                Err(e) => warn!(concat!("Error handling event ", $event, ": {:?}"), e),
                _ => {}
            };
        });
    };
}

impl Worker {
    pub async fn new() -> Result<Worker, Box<dyn Error>> {
        let intents = Intents::GUILDS
            | Intents::GUILD_MESSAGES
            | Intents::GUILD_MEMBERS
            | Intents::GUILD_MESSAGES
            | Intents::GUILD_MESSAGE_REACTIONS
            | Intents::DIRECT_MESSAGES
            | Intents::DIRECT_MESSAGE_REACTIONS;

        let token = std::env::var("DISCORD_TOKEN")?;
        let cluster = Cluster::builder(token, intents)
            .shard_scheme(ShardScheme::Auto)
            .http_client(
                Client::builder()
                    .proxy(std::env::var("DISCORD_PROXY")?, true)
                    .ratelimiter(None) // proxy does rate limiting by itself already
                    .build(),
            )
            .build()
            .await?;

        let db = Database::connect().await?;

        let cache = Cache::connect().await?;

        let forwarder = Forwarder::connect().await?;

        Ok(Worker {
            db,
            cluster,
            cache,
            forwarder,
            outstanding_member_requests: DashMap::new(),
        })
    }

    pub async fn run(self: Worker) {
        let mut events = self.cluster.some_events(
            EventTypeFlags::SHARD_PAYLOAD
                | EventTypeFlags::READY
                | EventTypeFlags::GUILD_CREATE
                | EventTypeFlags::GUILD_UPDATE
                | EventTypeFlags::GUILD_DELETE
                | EventTypeFlags::ROLE_CREATE
                | EventTypeFlags::ROLE_UPDATE
                | EventTypeFlags::ROLE_DELETE
                | EventTypeFlags::CHANNEL_CREATE
                | EventTypeFlags::CHANNEL_UPDATE
                | EventTypeFlags::CHANNEL_DELETE
                | EventTypeFlags::MEMBER_ADD
                | EventTypeFlags::MEMBER_UPDATE
                | EventTypeFlags::MEMBER_REMOVE
                | EventTypeFlags::MEMBER_CHUNK,
        );

        let arc = Arc::new(self);
        let self_status = arc.clone();

        // Bring the cluster up in a different worker, since we're
        // going to be processing events before this resolves.
        //
        // After spawn, go ahead and cycle through our presences.
        tokio::spawn(async move {
            info!("Starting cluster...");
            self_status.cluster.up().await;
            info!("Cluster online!");

            self_status.presence_loop().await;
        });

        while let Some((shard_id, event)) = events.next().await {
            if let Err(e) = arc.clone().handle_event(shard_id, event) {
                warn!("Error handling event on shard {}: {:?}", shard_id, e);
            }
        }
    }

    /// Handles a single event received from the gateway. Note that not all events
    /// possible are emitted here. In particular, events need to be included in both
    /// the intents and the event type flags in order for them to show up here.
    ///
    /// Note: this function is explicitly not async as to force usage of tokio::spawn
    /// whenever anything IO-related needs to happen. This is because this function is
    /// called inside the hot event loop and as a result needs to never block the task.
    #[inline(always)]
    fn handle_event(self: Arc<Worker>, shard: u64, event: Event) -> VoidResult {
        match event {
            Event::ShardPayload(payload) => {
                handle_event!("ShardPayload", self.forwarder.try_forward(payload).await);
            }

            Event::Ready(ready) => {
                info!("Shard {} ready with {} guilds!", shard, ready.guilds.len());

                for guild in ready.guilds {
                    if let GuildStatus::Online(g) = guild {
                        let worker = self.clone();

                        tokio::spawn(async move {
                            let _ = worker.handle_guild_created(&g, shard).await;
                        });
                    }
                }
            }

            Event::GuildCreate(guild) => {
                handle_event!(
                    "GuildCreate",
                    self.handle_guild_created(&guild, shard).await
                );
            }

            Event::GuildUpdate(guild) => {
                handle_event!(
                    "GuildUpdate",
                    self.cache
                        .update_guild(guild.id, |g| {
                            // Partial update, only these should really be used by us anyway.
                            g.name = guild.name.clone();
                            g.icon = guild.icon.clone();
                            g.owner_id = guild.owner_id;
                        })
                        .await
                );
            }

            // If unavailable is false, it means we got kicked. Otherwise we don't care.
            Event::GuildDelete(guild) if !guild.unavailable => {
                handle_event!("GuildDelete", self.handle_guild_deleted(guild.id).await);
            }

            Event::RoleCreate(role) => {
                handle_event!(
                    "RoleCreate",
                    self.cache
                        .update_guild(role.guild_id, |g| {
                            g.roles.push(role.role);
                        })
                        .await
                );
            }

            Event::RoleUpdate(role) => {
                handle_event!(
                    "RoleUpdate",
                    self.cache
                        .update_guild(role.guild_id, |g| {
                            g.roles
                                .iter_mut()
                                .filter(|g| g.id == role.role.id)
                                .for_each(|r| {
                                    *r = role.role.clone();
                                });
                        })
                        .await
                );
            }

            Event::RoleDelete(role) => {
                handle_event!(
                    "RoleDelete",
                    self.cache
                        .update_guild(role.guild_id, |g| {
                            g.roles.retain(|x| x.id != role.role_id);
                        })
                        .await
                );
            }

            Event::ChannelCreate(ChannelCreate(Channel::Guild(channel))) => {
                handle_event!(
                    "ChannelCreate",
                    self.cache
                        .update_guild(channel.guild_id().unwrap(), |g| {
                            g.channels.push(channel);
                        })
                        .await
                );
            }

            Event::ChannelUpdate(ChannelUpdate(Channel::Guild(channel))) => {
                handle_event!(
                    "ChannelUpdate",
                    self.cache
                        .update_guild(channel.guild_id().unwrap(), |g| {
                            g.channels
                                .iter_mut()
                                .filter(|g| g.id() == channel.id())
                                .for_each(|c| {
                                    *c = channel.clone();
                                });
                        })
                        .await
                );
            }

            Event::ChannelDelete(ChannelDelete(Channel::Guild(channel))) => {
                handle_event!(
                    "ChannelDelete",
                    self.cache
                        .update_guild(channel.guild_id().unwrap(), |g| {
                            g.channels.retain(|x| x.id() != channel.id());
                        })
                        .await
                );
            }

            Event::MemberAdd(member) => {
                handle_event!(
                    "MemberAdd",
                    self.db
                        .upsert_member(member.guild_id, member.user.id, &member.nick, &member.roles)
                        .await
                );
            }

            Event::MemberUpdate(update) => {
                handle_event!(
                    "MemberUpdate",
                    self.db
                        .upsert_member(update.guild_id, update.user.id, &update.nick, &update.roles)
                        .await
                );
            }

            Event::MemberRemove(removal) => {
                handle_event!(
                    "MemberRemove",
                    self.db
                        .remove_member(removal.guild_id, removal.user.id)
                        .await
                );
            }

            Event::MemberChunk(chunk) => {
                handle_event!("MemberChunk", self.handle_member_chunk(&chunk).await);
            }

            _ => {}
        };

        Ok(())
    }

    /// Invoked on a new task whenever a guild is "created", either through startup Ready,
    /// through startup guild creation, or whenever the bot joins a new server. Ensures that
    /// we fetch the full list of members.
    async fn handle_guild_created(self: Arc<Worker>, guild: &Guild, shard: u64) -> VoidResult {
        let has_all_members =
            guild.members.len() >= guild.member_count.map(|x| x as usize).unwrap_or(usize::MAX);

        debug!(
            "Joined guild {} with {} members.",
            guild.name,
            guild.members.len()
        );

        // Update cache.
        self.cache.upsert_guild(guild).await?;

        // If we already have all members, no need
        if has_all_members {
            debug!(
                "Received full set of members ({}) in initial guild creation for {} ({})",
                guild.members.len(),
                guild.name,
                guild.id.0
            );

            self.db.reset_guild(guild.id).await?;
            self.db
                .upsert_batch_members(guild.id, guild.members.iter())
                .await?;

            return Ok(());
        }

        debug!(
            "Received {}/{} members in initial guild creation for {} ({})",
            guild.members.len(),
            guild.member_count.map(|x| x as i64).unwrap_or(-1),
            guild.name,
            guild.id.0
        );

        // We don't have all members yet. Clear the guild info now and request the
        // full list from the gateway, which will be processed as they come in.
        self.db.reset_guild(guild.id).await?;

        // Request guild members from the gateway.
        self.cluster
            .command(
                shard,
                &RequestGuildMembers::builder(guild.id)
                    .presences(false)
                    .query("", None),
            )
            .await?;

        // Register that we're waiting for this.
        self.outstanding_member_requests
            .insert(guild.id, Instant::now());

        Ok(())
    }

    /// Invoked whenever some member chunk arrives. Note that we implicitly assume
    /// that all requests beforehand have deleted the old guild information so that
    /// we do not keep stale members in there that have already left.
    async fn handle_member_chunk(self: Arc<Worker>, chunk: &MemberChunk) -> VoidResult {
        debug!(
            "Received {}-long member chunk for {}",
            chunk.members.len(),
            chunk.guild_id.0
        );

        // if this is the last chunk, clear it from the outstanding member requests
        if chunk.chunk_index == chunk.chunk_count - 1 {
            self.outstanding_member_requests.remove(&chunk.guild_id);

            debug!(
                "Received all members for {}. Now have {} outstanding member requests that are still pending.",
                chunk.guild_id.0,
                self.outstanding_member_requests.len()
            );
        }

        self.db
            .upsert_batch_members(chunk.guild_id, chunk.members.iter())
            .await?;

        Ok(())
    }

    /// Invoked on a new task whenever a guild is deleted. We clean up both the cache
    /// and the membership for the guild.
    async fn handle_guild_deleted(self: Arc<Worker>, guild_id: GuildId) -> VoidResult {
        debug!("Got kicked from guild {}", guild_id.0);

        self.cache.delete_guild(guild_id).await?;
        self.db.reset_guild(guild_id).await?;

        Ok(())
    }

    /// Infinite loop that changes the presence of the bot between a set
    /// of preconfigured presences.
    async fn presence_loop(self: Arc<Worker>) {
        for &(ty, msg) in STATUSES.iter().cycle() {
            let message = format!(
                "{} \n{}Version {}",
                msg,
                "\u{3000}".repeat(118 - msg.len() - VERSION.len()),
                VERSION
            );

            let message = UpdateStatus::new(
                vec![Activity {
                    application_id: None,
                    assets: None,
                    created_at: None,
                    details: None,
                    emoji: None,
                    flags: None,
                    id: None,
                    instance: None,
                    kind: ty,
                    name: message,
                    party: None,
                    secrets: None,
                    state: None,
                    timestamps: None,
                    url: None,
                }],
                false,
                None,
                Status::Online,
            );

            for shard in self.cluster.shards() {
                match shard.command(&message).await {
                    Err(x) => warn!("Error setting presence: {:?}", x),
                    _ => {}
                }
            }

            // Wait for 10 minutes.
            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
        }
    }
}