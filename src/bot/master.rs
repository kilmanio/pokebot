use std::collections::HashMap;

use async_trait::async_trait;
use futures::future;
use rand::{rngs::SmallRng, seq::SliceRandom, SeedableRng};
use serde::{Deserialize, Serialize};
use slog::{error, info, o, trace, Logger};
use tsclientlib::{ClientId, ConnectOptions, Connection, Identity, MessageTarget};
use xtra::{spawn::Tokio, Actor, Address, Context, Handler, Message, WeakAddress};

use crate::audio_player::AudioPlayerError;
use crate::teamspeak::TeamSpeakConnection;

use crate::Args;

use crate::bot::{GetBotData, GetChannel, GetName, MusicBot, MusicBotArgs, MusicBotMessage};

pub struct MasterBot {
    config: MasterConfig,
    my_addr: Option<WeakAddress<Self>>,
    teamspeak: TeamSpeakConnection,
    available_names: Vec<String>,
    available_ids: Vec<Identity>,
    connected_bots: HashMap<String, Address<MusicBot>>,
    rng: SmallRng,
    logger: Logger,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MasterArgs {
    #[serde(default = "default_name")]
    pub master_name: String,
    pub address: String,
    pub channel: Option<String>,
    #[serde(default = "default_verbose")]
    pub verbose: u8,
    pub domain: String,
    pub bind_address: String,
    pub webserver_enable: bool,
    pub names: Vec<String>,
    pub id: Option<Identity>,
    pub ids: Option<Vec<Identity>>,
}

impl MasterBot {
    pub async fn spawn(args: MasterArgs, logger: Logger) -> Address<Self> {
        info!(logger, "Starting in TeamSpeak mode");

        let mut con_config = Connection::build(args.address.clone())
            .logger(logger.clone())
            .version(tsclientlib::Version::Linux_3_3_2)
            .name(args.master_name.clone())
            .identity(args.id.expect("identity should exist"))
            .log_commands(args.verbose >= 1)
            .log_packets(args.verbose >= 2)
            .log_udp_packets(args.verbose >= 3);

        if let Some(channel) = args.channel {
            con_config = con_config.channel(channel);
        }

        let connection = TeamSpeakConnection::new(logger.clone()).await.unwrap();
        trace!(logger, "Created teamspeak connection");

        let config = MasterConfig {
            master_name: args.master_name,
            address: args.address,
            verbose: args.verbose,
        };

        let bot_addr = Self {
            config,
            my_addr: None,
            teamspeak: connection,
            logger: logger.clone(),
            rng: SmallRng::from_entropy(),
            available_names: args.names,
            available_ids: args.ids.expect("identities"),
            connected_bots: HashMap::new(),
        }
        .create(None)
        .spawn(&mut Tokio::Global);

        bot_addr.send(Connect(con_config)).await.unwrap().unwrap();
        trace!(logger, "Spawned master bot actor");

        bot_addr
    }

    async fn bot_args_for_client(
        &mut self,
        user_id: ClientId,
    ) -> Result<MusicBotArgs, BotCreationError> {
        let channel = match self.teamspeak.channel_of_user(user_id).await {
            Some(channel) => channel,
            None => return Err(BotCreationError::UnfoundUser),
        };

        if channel == self.teamspeak.current_channel().await.unwrap() {
            return Err(BotCreationError::MasterChannel(
                self.config.master_name.clone(),
            ));
        }

        for bot in self.connected_bots.values() {
            if bot.send(GetChannel).await.unwrap() == Some(channel) {
                return Err(BotCreationError::MultipleBots(
                    bot.send(GetName).await.unwrap(),
                ));
            }
        }

        let channel_path = self
            .teamspeak
            .channel_path_of_user(user_id)
            .await
            .expect("can find poke sender");

        self.available_names.shuffle(&mut self.rng);
        let name = match self.available_names.pop() {
            Some(v) => v,
            None => {
                return Err(BotCreationError::OutOfNames);
            }
        };

        self.available_ids.shuffle(&mut self.rng);
        let identity = match self.available_ids.pop() {
            Some(v) => v,
            None => {
                return Err(BotCreationError::OutOfIdentities);
            }
        };

        Ok(MusicBotArgs {
            name: name.clone(),
            master: self.my_addr.clone(),
            address: self.config.address.clone(),
            identity,
            local: false,
            channel: channel_path,
            verbose: self.config.verbose,
            logger: self.logger.new(o!("musicbot" => name)),
        })
    }

    async fn spawn_bot_for_client(&mut self, id: ClientId) {
        match self.bot_args_for_client(id).await {
            Ok(bot_args) => {
                let name = bot_args.name.clone();
                let bot = MusicBot::spawn(bot_args).await;
                self.connected_bots.insert(name, bot);
            }
            Err(e) => self.teamspeak.send_message_to_user(id, e.to_string()).await,
        }
    }

    async fn on_message(&mut self, message: MusicBotMessage) -> Result<(), AudioPlayerError> {
        match message {
            MusicBotMessage::TextMessage(message) => {
                if let MessageTarget::Poke(who) = message.target {
                    info!(
                        self.logger,
                        "Poked, creating bot"; "user" => %who
                    );
                    self.spawn_bot_for_client(who).await;
                }
            }
            MusicBotMessage::ClientAdded(id) => {
                if id == self.teamspeak.my_id().await {
                    self.teamspeak
                        .set_description(String::from("Poke me if you want a music bot!"))
                        .await;
                }
            }
            _ => (),
        }

        Ok(())
    }

    pub async fn bot_data(&self, name: String) -> Option<crate::web_server::BotData> {
        let bot = self.connected_bots.get(&name)?;

        bot.send(GetBotData).await.ok()
    }

    pub async fn bot_datas(&self) -> Vec<crate::web_server::BotData> {
        let len = self.connected_bots.len();
        let mut result = Vec::with_capacity(len);
        for bot in self.connected_bots.values() {
            let bot_data = bot.send(GetBotData).await.unwrap();
            result.push(bot_data);
        }

        result
    }

    pub fn bot_names(&self) -> Vec<String> {
        let len = self.connected_bots.len();
        let mut result = Vec::with_capacity(len);
        for name in self.connected_bots.keys() {
            result.push(name.clone());
        }

        result
    }

    fn on_bot_disconnect(&mut self, name: String, id: Identity) {
        self.connected_bots.remove(&name);
        self.available_names.push(name);
        self.available_ids.push(id);
    }

    pub async fn quit(&mut self, reason: String) -> Result<(), tsclientlib::Error> {
        let futures = self
            .connected_bots
            .values()
            .map(|b| b.send(Quit(reason.clone())));
        for res in future::join_all(futures).await {
            if let Err(e) = res {
                error!(self.logger, "Failed to shut down bot"; "error" => %e);
            }
        }
        self.teamspeak.disconnect(&reason).await
    }
}

#[async_trait]
impl Actor for MasterBot {
    async fn started(&mut self, ctx: &mut Context<Self>) {
        self.my_addr = Some(ctx.address().unwrap().downgrade());
    }
}

pub struct Connect(pub ConnectOptions);
impl Message for Connect {
    type Result = Result<(), tsclientlib::Error>;
}

#[async_trait]
impl Handler<Connect> for MasterBot {
    async fn handle(
        &mut self,
        opt: Connect,
        ctx: &mut Context<Self>,
    ) -> Result<(), tsclientlib::Error> {
        let addr = ctx.address().unwrap();
        self.teamspeak.connect_for_bot(opt.0, addr.downgrade())?;
        Ok(())
    }
}

pub struct Quit(pub String);
impl Message for Quit {
    type Result = Result<(), tsclientlib::Error>;
}

#[async_trait]
impl Handler<Quit> for MasterBot {
    async fn handle(&mut self, q: Quit, _: &mut Context<Self>) -> Result<(), tsclientlib::Error> {
        self.quit(q.0).await
    }
}

pub struct BotDisonnected {
    pub name: String,
    pub identity: Identity,
}

impl Message for BotDisonnected {
    type Result = ();
}

#[async_trait]
impl Handler<BotDisonnected> for MasterBot {
    async fn handle(&mut self, dc: BotDisonnected, _: &mut Context<Self>) {
        self.on_bot_disconnect(dc.name, dc.identity);
    }
}

#[async_trait]
impl Handler<MusicBotMessage> for MasterBot {
    async fn handle(
        &mut self,
        msg: MusicBotMessage,
        _: &mut Context<Self>,
    ) -> Result<(), AudioPlayerError> {
        self.on_message(msg).await
    }
}

#[derive(Debug)]
pub enum BotCreationError {
    UnfoundUser,
    MasterChannel(String),
    MultipleBots(String),
    OutOfNames,
    OutOfIdentities,
}

impl std::fmt::Display for BotCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use BotCreationError::*;
        match self {
            UnfoundUser => write!(
                f,
                "I can't find you in the channel list, \
                    either I am not subscribed to your channel or this is a bug.",
            ),
            MasterChannel(name) => write!(f, "Joining the channel of \"{}\" is not allowed", name),
            MultipleBots(name) => write!(
                f,
                "\"{}\" is already in this channel. \
                         Multiple bots in one channel are not allowed.",
                name
            ),
            OutOfNames => write!(f, "Out of names. Too many bots are already connected!"),
            OutOfIdentities => write!(f, "Out of identities. Too many bots are already connected!"),
        }
    }
}

fn default_name() -> String {
    String::from("PokeBot")
}

fn default_verbose() -> u8 {
    0
}

impl MasterArgs {
    pub fn merge(self, args: Args) -> Self {
        let address = args.address.unwrap_or(self.address);
        let channel = args.master_channel.or(self.channel);
        let verbose = if args.verbose > 0 {
            args.verbose
        } else {
            self.verbose
        };

        Self {
            master_name: self.master_name,
            names: self.names,
            ids: self.ids,
            address,
            domain: self.domain,
            bind_address: self.bind_address,
            webserver_enable: self.webserver_enable,
            id: self.id,
            channel,
            verbose,
        }
    }
}

pub struct MasterConfig {
    pub master_name: String,
    pub address: String,
    pub verbose: u8,
}
