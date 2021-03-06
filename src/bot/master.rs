use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use log::info;
use rand::{rngs::SmallRng, seq::SliceRandom, SeedableRng};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tsclientlib::{ClientId, Connection, Identity, MessageTarget};

use crate::audio_player::AudioPlayerError;
use crate::teamspeak::TeamSpeakConnection;

use crate::Args;

use crate::bot::{MusicBot, MusicBotArgs, MusicBotMessage};

pub struct MasterBot {
    config: Arc<MasterConfig>,
    music_bots: Arc<RwLock<MusicBots>>,
    teamspeak: TeamSpeakConnection,
    sender: Arc<RwLock<UnboundedSender<MusicBotMessage>>>,
}

struct MusicBots {
    rng: SmallRng,
    available_names: Vec<usize>,
    available_ids: Vec<usize>,
    connected_bots: HashMap<String, Arc<MusicBot>>,
}

impl MasterBot {
    pub async fn new(args: MasterArgs) -> (Arc<Self>, impl Future) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = Arc::new(RwLock::new(tx));
        info!("Starting in TeamSpeak mode");

        let mut con_config = Connection::build(args.address.clone())
            .version(tsclientlib::Version::Linux_3_3_2)
            .name(args.master_name.clone())
            .identity(args.id.expect("identity should exist"))
            .log_commands(args.verbose >= 1)
            .log_packets(args.verbose >= 2)
            .log_udp_packets(args.verbose >= 3);

        if let Some(channel) = args.channel {
            con_config = con_config.channel(channel);
        }

        let connection = TeamSpeakConnection::new(tx.clone(), con_config)
            .await
            .unwrap();

        let config = Arc::new(MasterConfig {
            master_name: args.master_name,
            address: args.address,
            names: args.names,
            ids: args.ids.expect("identies should exists"),
            local: args.local,
            verbose: args.verbose,
        });

        let name_count = config.names.len();
        let id_count = config.ids.len();

        let music_bots = Arc::new(RwLock::new(MusicBots {
            rng: SmallRng::from_entropy(),
            available_names: (0..name_count).collect(),
            available_ids: (0..id_count).collect(),
            connected_bots: HashMap::new(),
        }));

        let bot = Arc::new(Self {
            config,
            music_bots,
            teamspeak: connection,
            sender: tx.clone(),
        });

        let cbot = bot.clone();
        let msg_loop = async move {
            'outer: loop {
                while let Some(msg) = rx.recv().await {
                    match msg {
                        MusicBotMessage::Quit(reason) => {
                            let mut cteamspeak = cbot.teamspeak.clone();
                            cteamspeak.disconnect(&reason).await;
                            break 'outer;
                        }
                        MusicBotMessage::ClientDisconnected { id, .. } => {
                            if id == cbot.my_id().await {
                                // TODO Reconnect since quit was not called
                                break 'outer;
                            }
                        }
                        _ => cbot.on_message(msg).await.unwrap(),
                    }
                }
            }
        };

        (bot, msg_loop)
    }

    async fn build_bot_args_for(&self, id: ClientId) -> Result<MusicBotArgs, BotCreationError> {
        let mut cteamspeak = self.teamspeak.clone();
        let channel = match cteamspeak.channel_of_user(id).await {
            Some(channel) => channel,
            None => return Err(BotCreationError::UnfoundUser),
        };

        if channel == cteamspeak.my_channel().await {
            return Err(BotCreationError::MasterChannel(
                self.config.master_name.clone(),
            ));
        }

        let MusicBots {
            ref mut rng,
            ref mut available_names,
            ref mut available_ids,
            ref connected_bots,
        } = &mut *self.music_bots.write().expect("RwLock was not poisoned");

        for bot in connected_bots.values() {
            if bot.my_channel().await == channel {
                return Err(BotCreationError::MultipleBots(bot.name().to_owned()));
            }
        }

        let channel_path = cteamspeak
            .channel_path_of_user(id)
            .await
            .expect("can find poke sender");

        available_names.shuffle(rng);
        let name_index = match available_names.pop() {
            Some(v) => v,
            None => {
                return Err(BotCreationError::OutOfNames);
            }
        };
        let name = self.config.names[name_index].clone();

        available_ids.shuffle(rng);
        let id_index = match available_ids.pop() {
            Some(v) => v,
            None => {
                return Err(BotCreationError::OutOfIdentities);
            }
        };

        let id = self.config.ids[id_index].clone();

        let cmusic_bots = self.music_bots.clone();
        let disconnect_cb = Box::new(move |n, name_index, id_index| {
            let mut music_bots = cmusic_bots.write().expect("RwLock was not poisoned");
            music_bots.connected_bots.remove(&n);
            music_bots.available_names.push(name_index);
            music_bots.available_ids.push(id_index);
        });

        info!("Connecting to {} on {}", channel_path, self.config.address);

        Ok(MusicBotArgs {
            name,
            name_index,
            id_index,
            local: self.config.local,
            address: self.config.address.clone(),
            id,
            channel: channel_path,
            verbose: self.config.verbose,
            disconnect_cb,
        })
    }

    async fn spawn_bot_for(&self, id: ClientId) {
        match self.build_bot_args_for(id).await {
            Ok(bot_args) => {
                let (bot, fut) = MusicBot::new(bot_args).await;
                tokio::spawn(fut);
                let mut music_bots = self.music_bots.write().expect("RwLock was not poisoned");
                music_bots
                    .connected_bots
                    .insert(bot.name().to_string(), bot);
            }
            Err(e) => {
                let mut cteamspeak = self.teamspeak.clone();
                cteamspeak.send_message_to_user(id, e.to_string()).await
            }
        }
    }

    async fn on_message(&self, message: MusicBotMessage) -> Result<(), AudioPlayerError> {
        match message {
            MusicBotMessage::TextMessage(message) => {
                if let MessageTarget::Poke(who) = message.target {
                    info!("Poked by {}, creating bot for their channel", who);
                    self.spawn_bot_for(who).await;
                }
            }
            MusicBotMessage::ChannelAdded(id) => {
                let mut cteamspeak = self.teamspeak.clone();
                cteamspeak.subscribe(id).await;
            }
            MusicBotMessage::ClientAdded(id) => {
                let mut cteamspeak = self.teamspeak.clone();

                if id == cteamspeak.my_id().await {
                    cteamspeak
                        .set_description(String::from("Poke me if you want a music bot!"))
                        .await;
                }
            }
            _ => (),
        }

        Ok(())
    }

    async fn my_id(&self) -> ClientId {
        let mut cteamspeak = self.teamspeak.clone();

        cteamspeak.my_id().await
    }

    pub fn bot_data(&self, name: String) -> Option<crate::web_server::BotData> {
        let music_bots = self.music_bots.read().unwrap();
        let bot = music_bots.connected_bots.get(&name)?;

        Some(crate::web_server::BotData {
            name,
            state: bot.state(),
            volume: bot.volume(),
            position: bot.position(),
            currently_playing: bot.currently_playing(),
            playlist: bot.playlist_to_vec(),
        })
    }

    pub fn bot_datas(&self) -> Vec<crate::web_server::BotData> {
        let music_bots = self.music_bots.read().unwrap();

        let len = music_bots.connected_bots.len();
        let mut result = Vec::with_capacity(len);
        for (name, bot) in &music_bots.connected_bots {
            let bot_data = crate::web_server::BotData {
                name: name.clone(),
                state: bot.state(),
                volume: bot.volume(),
                position: bot.position(),
                currently_playing: bot.currently_playing(),
                playlist: bot.playlist_to_vec(),
            };

            result.push(bot_data);
        }

        result
    }

    pub fn bot_names(&self) -> Vec<String> {
        let music_bots = self.music_bots.read().unwrap();

        let len = music_bots.connected_bots.len();
        let mut result = Vec::with_capacity(len);
        for name in music_bots.connected_bots.keys() {
            result.push(name.clone());
        }

        result
    }

    pub fn quit(&self, reason: String) {
        let music_bots = self.music_bots.read().unwrap();
        for bot in music_bots.connected_bots.values() {
            bot.quit(reason.clone())
        }
        let sender = self.sender.read().unwrap();
        sender.send(MusicBotMessage::Quit(reason)).unwrap();
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

#[derive(Debug, Serialize, Deserialize)]
pub struct MasterArgs {
    #[serde(default = "default_name")]
    pub master_name: String,
    #[serde(default = "default_local")]
    pub local: bool,
    pub address: String,
    pub channel: Option<String>,
    #[serde(default = "default_verbose")]
    pub verbose: u8,
    pub domain: String,
    pub bind_address: String,
    pub names: Vec<String>,
    pub id: Option<Identity>,
    pub ids: Option<Vec<Identity>>,
}

fn default_name() -> String {
    String::from("PokeBot")
}

fn default_local() -> bool {
    false
}

fn default_verbose() -> u8 {
    0
}

impl MasterArgs {
    pub fn merge(self, args: Args) -> Self {
        let address = args.address.unwrap_or(self.address);
        let local = args.local || self.local;
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
            local,
            address,
            domain: self.domain,
            bind_address: self.bind_address,
            id: self.id,
            channel,
            verbose,
        }
    }
}

pub struct MasterConfig {
    pub master_name: String,
    pub address: String,
    pub names: Vec<String>,
    pub ids: Vec<Identity>,
    pub local: bool,
    pub verbose: u8,
}
