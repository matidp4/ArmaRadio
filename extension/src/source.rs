use std::{
    collections::HashMap,
    mem::MaybeUninit,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex, RwLock, atomic::AtomicU8,
    },
    time::SystemTime,
};

use alto::Source;
use arma_rs::{Context, Group, ContextState};
use crossbeam_channel::TryRecvError;

use crate::{
    listener::Listener,
    streams::{StreamPacket, Streams},
    vector3::Vector3,
};

pub struct Sources();

type SourceMap = RwLock<HashMap<String, Mutex<SoundSource>>>;

impl Sources {
    pub fn get() -> Arc<SourceMap> {
        static mut SINGLETON: MaybeUninit<Arc<SourceMap>> = MaybeUninit::uninit();
        static mut INIT: bool = false;

        unsafe {
            if !INIT {
                println!("new source map");
                SINGLETON.write(Arc::new(RwLock::new(HashMap::new())));
                INIT = true;
            }
            SINGLETON.assume_init_ref().clone()
        }
    }
}

enum SoundCommand {
    SetPos(Vector3, Vector3),
    SetGain(f32),
    RefreshGain,
    Destroy,
}

#[derive(Debug)]
pub struct SoundSource {
    position: Vector3,
    time: SystemTime,
    channel: Sender<SoundCommand>,
}

impl SoundSource {
    pub fn new(ctx: Context, id: String, url: String, gain: f32) -> Self {
        let (tx, rx): (Sender<SoundCommand>, Receiver<SoundCommand>) = mpsc::channel();
        std::thread::spawn(move || {
            println!("Starting source `{}`", id);
            let stream = Streams::listen(url);
            let Ok(mut source) = Listener::get().new_streaming_source() else {
                println!("Error creating source");
                return;
            };
            source
                .set_soft_spatialization(alto::SoftSourceSpatialization::Enabled)
                .expect("Error setting soft spatialization");
            source.set_gain(gain * ctx.group().get::<AtomicU8>().map(|gain| gain.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(255) as f32 / 255.0).expect("Error setting gain");
            let mut specific_gain = gain;
            'outer: loop {
                while let Ok(command) = rx.try_recv() {
                    match command {
                        #[allow(unused_variables)]
                        SoundCommand::SetPos(pos, vel) => {
                            if source.set_position([pos.x, pos.y, pos.z]).is_err() {
                                println!("Error setting position");
                            }
                            if cfg!(not(test))
                                && source.set_velocity([vel.x, vel.y, vel.z]).is_err()
                            {
                                println!("Error setting velocity");
                            }
                        }
                        SoundCommand::SetGain(gain) => {
                            println!("Setting gain to {}", gain);
                            specific_gain = gain;
                            if source.set_gain(gain * ctx.group().get::<AtomicU8>().map(|gain| gain.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(255) as f32 / 255.0).is_err() {
                                println!("Error setting gain");
                            }
                        }
                        SoundCommand::RefreshGain => {
                            println!("Refreshing gain");
                            if source.set_gain(specific_gain * ctx.group().get::<AtomicU8>().map(|gain| gain.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(255) as f32 / 255.0).is_err() {
                                println!("Error setting gain");
                            }
                        }
                        SoundCommand::Destroy => {
                            println!("Source `{}` has been told to destroy", id);
                            source.stop();
                            break 'outer;
                        }
                    }
                }
                match stream.receiver.try_recv() {
                    Ok(recv) => {
                        match recv {
                            StreamPacket::Data(samples, freq) => {
                                if freq != 44100 {
                                    println!("Received data with frequency {}", freq);
                                }
                                let buffer = if source.buffers_processed() > 200 {
                                    if let Ok(mut buffer) = source.unqueue_buffer() {
                                        if samples.len() != 1152 {
                                            println!("samples len: {}", samples.len());
                                        }
                                        if let Err(e) = buffer.set_data(samples, freq) {
                                            println!("Error setting buffer sample data: {}", e);
                                            continue;
                                        }
                                        buffer
                                    } else {
                                        let Ok(buffer) = Listener::get().new_buffer(samples, freq) else {
                                            println!("Error creating buffer");
                                            continue;
                                        };
                                        buffer
                                    }
                                } else {
                                    let Ok(buffer) = Listener::get().new_buffer(samples, freq) else {
                                        println!("Error creating buffer");
                                        continue;
                                    };
                                    buffer
                                };
                                source.queue_buffer(buffer).expect("Error queueing buffer");
                                if source.state() != alto::SourceState::Playing
                                    && source.buffers_queued() > 75
                                {
                                    println!("Playing source, {:?}", source.state());
                                    source.play();
                                }
                            }
                            StreamPacket::Title(title) => {
                                println!("Received title: {:?}", title);
                                if ctx
                                    .callback_data(
                                        "live_radio",
                                        "title",
                                        Some(vec![id.to_string(), title]),
                                    )
                                    .is_err()
                                {
                                    // arma is probably closed
                                    break;
                                }
                            }
                            StreamPacket::Close => {
                                println!("Stream closed");
                                source.stop();
                                break;
                            }
                            StreamPacket::Check => {
                                // noop
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {},
                    Err(TryRecvError::Disconnected) => {
                        println!("Stream receiver disconnected");
                        break;
                    }
                }
            }
            // std::thread::sleep(std::time::Duration::from_millis(3000));
            println!("Source `{}` has died", id);
        });
        Self {
            position: Vector3::new(0.0, 0.0, 0.0),
            time: SystemTime::now(),
            channel: tx,
        }
    }

    pub fn set_position(&mut self, position: [f32; 3]) {
        let old = self.time;
        self.time = SystemTime::now();
        let dif = self
            .time
            .duration_since(old)
            .expect("time doesn't flow backwards");
        let elapsed: f32 = (dif.as_secs() as f32) + (dif.subsec_nanos() as f32 / 1_000_000_000.0);

        if elapsed == 0.0 {
            return;
        }

        let velocity = self
            .position
            .update(position[0], position[1], position[2], elapsed);
        if self
            .channel
            .send(SoundCommand::SetPos(self.position, velocity))
            .is_err()
        {
            println!("Error sending position update");
        }
    }

    pub fn set_gain(&mut self, gain: f32) {
        if self.channel.send(SoundCommand::SetGain(gain)).is_err() {
            println!("error sending gain update");
        }
    }

    pub fn refresh_gain(&mut self) {
        self.channel.send(SoundCommand::RefreshGain).expect("not poisoned");
    }
}

impl Drop for SoundSource {
    fn drop(&mut self) {
        println!("Dropping source");
        if self.channel.send(SoundCommand::Destroy).is_err() {
            println!("Error sending destroy command");
        }
    }
}

pub fn cleanup() {
    println!("cleaning up sources");
    Sources::get().write().expect("not poisoned").clear();
}

pub fn group() -> Group {
    let global_gain = AtomicU8::new(255);
    Group::new()
        .command("new", command_new)
        .command("destroy", command_destroy)
        .command("pos", command_set_position)
        .command("gain", command_set_gain)
        .command("global_gain", command_set_global_gain)
        .state(global_gain)
}

fn command_new(ctx: Context, id: String, source: String, gain: f32) -> String {
    Sources::get().write().expect("not poisoned").insert(
        id.clone(),
        Mutex::new(SoundSource::new(ctx, id.clone(), source, gain)),
    );
    println!(
        "there is now {} sources",
        Sources::get().read().expect("not poisoned").len()
    );
    id
}

fn command_destroy(id: String) -> bool {
    println!("`{}` has been told to die", id);
    Sources::get()
        .write()
        .expect("not poisoned")
        .remove(&id)
        .is_some()
}

pub fn command_set_position(id: String, x: f32, y: f32, z: f32) {
    if let Some(src) = Sources::get().read().expect("not poisoned").get(&id) {
        src.lock().expect("not poisoned").set_position([x, y, z]);
    }
}

pub fn command_set_gain(id: String, gain: f32) {
    if let Some(src) = Sources::get().read().expect("not poisoned").get(&id) {
        src.lock().expect("not poisoned").set_gain(gain);
    }
}

pub fn command_set_global_gain(ctx: Context, gain: f32) {
    let gain = (gain * 255.0) as u8;
    println!("Setting global gain to {}", gain);
    if let Some(state) = ctx.group().get::<AtomicU8>(){
        state.store(gain, std::sync::atomic::Ordering::Relaxed);
    }
    Sources::get().read().expect("not poisoned").iter().for_each(|(_, src)| {
        src.lock().expect("not poisoned").refresh_gain();
    });
}
