use std::{
    collections::HashMap,
    env,
    process::Stdio,
    sync::Arc
};
use serenity::{
    client::Context,
    model::{
        misc::Mentionable,
        prelude::UserId,
    },
};
use songbird::{
    CoreEvent,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::Mutex,
    task,
};
use crate::structs::*;
use serenity::model::guild::Guild;
use serenity::model::id::ChannelId;
use serde_json::value::Value::Bool;

pub async fn join(ctx: &Context, response: Response) {
    let (guild, _) = response.guild(ctx).await;
    let user_channel = guild.voice_states.get(&response.member()).and_then(|vs| vs.channel_id);
    match user_channel {
        None => {
            response.edit(ctx, "Error: you first need to be in a voice channel").await;
        }
        Some(user_channel_id) => {
            match move_to(ctx, guild, user_channel_id).await {
                Ok(()) => {
                    response.follow_up(ctx, &format!("Joined {}", user_channel_id.mention())[..]).await;
                },
                Err(()) => {
                    response.follow_up(ctx, &format!("Error: couldn't join {}", user_channel_id.mention())[..]).await;
                },
            }
        }
    }
}

pub async fn leave(ctx: &Context, response: Response) {
    let (guild, guild_id) = response.guild(ctx).await;
    if let Some(current_state) = guild
        .voice_states
        .get(&ctx.cache.current_user_id().await)
    {
        if let Some(channel_id) = guild
            .voice_states
            .get(&response.member())
            .and_then(|vs| vs.channel_id)
            .filter(|user_channel_id| *user_channel_id == current_state.channel_id.unwrap())
        {
            let manager = songbird::get(ctx).await
                .expect("Songbird Voice client placed in at initialisation.").clone();
            if let Some(call) = manager.get(guild_id) {
                if let Ok(_) = call.lock().await.leave().await {
                    //response.delete(ctx);
                    response.follow_up(ctx, &format!("Left {}", channel_id.mention())[..]).await;
                } else {
                    response.edit(ctx, &format!("Error: Could not leave {}", channel_id.mention())[..]).await;
                }
            } else {
                response.edit(ctx, "Error: The bot is not in a call").await;
            }
            // to prevent poison errors, whenever the bot leaves it deletes the buffer for the server
            {
                let data_read = ctx.data.read().await;
                let _ = data_read.get::<JoinFlag>().expect("Typemap incomplete").clone().lock().await.insert(guild_id);
                let buffers_lock = data_read.get::<Lobbies>().expect("Typemap incomplete").clone();
                buffers_lock.write().await.remove(&guild_id);
            };
        } else {
            response.edit(ctx, "Error: You have to be in the same channel as the bot to remove it").await;
        }
    } else {
        response.edit(ctx, "Error: The bot is not in a voice channel").await;
    }
}

pub async fn dump(ctx: &Context, response: Response) {
    let (guild, guild_id) = response.guild(ctx).await;
    let members = guild.members;
    let data_read = ctx.data.read().await;
    let lobbies_lock = data_read.get::<Lobbies>().expect("Typemap incomplete").clone();
    if let Some(lobby_lock) = lobbies_lock.read().await.get(&guild_id).clone() {
        let lobby = lobby_lock.0.lock().await;
        let ssrc_map = lobby_lock.1.lock().await;
        let encoded_buffers = Arc::new(Mutex::new(Vec::<(Vec<u8>, String)>::new()));
        let mut encoding_threads = Vec::new();
        let output_format = output_format();
        for (id, audio_state_buffer) in lobby.iter() {
            if let Some(user_id) = ssrc_map.get(&id) {
                if let Some(member) = &members.get(user_id) {
                    let options = &response.data().as_ref().unwrap().options;
                    let mut insert_pauses = true;
                    for option in options {
                        match &option.name[..] {
                            "pauses" => {
                                if let Some(Bool(val)) = option.value {
                                    insert_pauses = val;
                                }
                            },
                            _ => {}
                        }
                    }

                    let buffer: Vec<i16>;
                    if insert_pauses {
                        buffer = audio_state_buffer.pop_uncompressed()
                    } else {
                        buffer = audio_state_buffer.pop_compressed()
                    }
                    let name = member.user.name.clone();
                    let encoded_buffers_clone = encoded_buffers.clone();
                    let output_format = output_format.clone();
                    encoding_threads.push(
                        task::spawn(async move {
                            let mut child = Command::new("ffmpeg")
                                .args(
                                    &[
                                        "-f", "s16be", // format in input
                                        "-ac", "2", // audio channels in input
                                        "-ar", "48k", // audio rate
                                        "-i", "-", // input takes a pipe
                                        "-f", &output_format[..], // output format
                                        "-b:a", "96k", // output rate
                                        "-ac", "2", // output audio channels
                                        "-" // output takes a pipe
                                    ])
                                .stdin(Stdio::piped())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null())
                                .spawn().expect("could not spawn encoder");

                            let samples = get_bytes(&buffer);
                            let mut stdin = child.stdin.take().expect("failed to open stdin");
                            task::spawn(async move {
                                stdin.write_all(&samples[..]).await.unwrap();
                            });
                            let encoded = child.wait_with_output().await.unwrap().stdout;
                            encoded_buffers_clone.lock().await.push((encoded, format!("{}.{}", name, output_format)));
                        }));
                }
            }
        };

        for handle in encoding_threads.drain(..) {
            handle.await.unwrap();
        }

        response.edit(ctx, "Done!").await;
        response.follow_up_files(ctx, &*encoded_buffers.clone().lock().await).await;
    };
}

pub async fn clear(ctx: &Context, response: Response) {
    let (_, guild_id) = response.guild(ctx).await;
    {
        let data_read = ctx.data.read().await;
        let lobbies_lock = data_read.get::<Lobbies>().expect("Typemap incomplete").clone();
        let lobby_lock = lobbies_lock.read().await.get(&guild_id).expect("could not acquire a read lock on the data").clone();
        let buffer = &mut lobby_lock.0.lock().await;
        buffer.clear();
    }
    //response.delete(ctx);
    response.follow_up(ctx, "The buffer has been cleared. No need to thank me").await;
}

// make the bot follow the user who calls this
pub async fn follow(ctx: &Context, response: Response) {
    let user_id = response.member();
    let (guild, guild_id) = response.guild(ctx).await;
    {
        let data_read = ctx.data.read().await;
        let follow_map = data_read.get::<FollowFlag>().expect("Typemap incomplete").clone();
        follow_map.lock().await.insert(guild_id, user_id);
    };
    response.follow_up(ctx, &format!("The bot will now follow {}", user_id.mention())[..]).await;

    let user_channel = guild.voice_states.get(&user_id).and_then(|vs| vs.channel_id);
    if let Some(user_channel_id) = user_channel {
        let _ = move_to(ctx, guild, user_channel_id).await;
    }
}

pub async fn unfollow(ctx: &Context, response: Response) {
    let user_id = response.member();
    let (_, guild_id) = response.guild(ctx).await;
    {
        let data_read = ctx.data.read().await;
        let follow_map = data_read.get::<FollowFlag>().expect("Typemap incomplete").clone();
        let mut follow_map_lock = follow_map.lock().await;
        match follow_map_lock.get(&guild_id) {
            Some(mapped_user_id) if *mapped_user_id == user_id => {
                let _ = follow_map_lock.remove(&guild_id);
                response.follow_up(ctx, &format!("The bot has stopped following {}.", user_id.mention())[..]).await;
            },
            _ => {
                response.follow_up(ctx, &format!("I don't even know who {} is.", user_id.mention())[..]).await;
            },
        }
    };
}

pub async fn move_to(ctx: &Context, guild: Guild, target_channel_id: ChannelId) -> Result<(),()> {
    let guild_id = guild.id;
    if let Some(current_channel_id) = guild.voice_states.get(&ctx.cache.current_user_id().await).and_then(|vs| vs.channel_id) {
        if current_channel_id == target_channel_id {
            return Ok(());
        }
    }
    let data_read = ctx.data.read().await;
    let join_flag = data_read.get::<JoinFlag>().expect("Typemap incomplete").clone();
    let _ = join_flag.lock().await.insert(guild_id);
    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    let (handler_lock, conn_result) = manager.join(guild_id, target_channel_id).await;

    return if let Ok(_) = conn_result {
        let audio_buffer: HashMap<u32, Buffer> = HashMap::new();
        let ssrc_map: HashMap<u32, UserId> = HashMap::new();
        let lobby = Arc::new((Mutex::new(audio_buffer), Mutex::new(ssrc_map)));
        let buffers_lock = data_read.get::<Lobbies>().expect("Typemap incomplete").clone();
        buffers_lock.write().await.insert(guild_id, lobby.clone());

        // NOTE: this skips listening for the actual connection result.
        let mut handler = handler_lock.lock().await;

        handler.add_global_event(
            CoreEvent::VoicePacket.into(),
            Receiver::new(lobby.clone()),
        );
        handler.add_global_event(
            CoreEvent::SpeakingStateUpdate.into(),
            Receiver::new(lobby.clone()),
        );
        handler.add_global_event(
            CoreEvent::SpeakingUpdate.into(),
            Receiver::new(lobby.clone()),
        );
        handler.add_global_event(
            CoreEvent::ClientDisconnect.into(),
            Receiver::new(lobby.clone()),
        );
        Ok(())
    } else {
        Err(())
    }
}

fn get_bytes(origin: &Vec<i16>) -> Vec<u8> {
    let mut output = Vec::new();
    origin.iter().for_each(|&signal| signal.to_be_bytes().iter().for_each(|&byte| { output.push(byte) }));
    output
}

fn output_format() -> String {
    match env::var("DISCORD_OUTPUT_FORMAT") {
        Ok(custom_format) => custom_format,
        Err(_) => "ogg".to_string()
    }
}