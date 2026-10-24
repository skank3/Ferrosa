use std::{f64::consts::PI, io::Cursor, num::NonZero, path::Path};

use bevy::{platform::collections::HashMap, prelude::*};
use ferrosa_protocol::clientbound::game::{GameState, ItemKind, events::sound::Sound};
use ferrosa_world::skeleton::BoneId;
use rodio::{
    Decoder, Source, SpatialPlayer,
    buffer::SamplesBuffer,
    mixer::{self, Mixer},
};

use super::{
    GAME_FPS,
    camera::GameCamera,
    netgame::{NetEvent, NetGame},
    smoothing::Smoothing,
};
use crate::{
    audio::Speakers,
    character_model::HumanPose,
    config::Config,
    game::{
        human_sync::{NetHuman, bone_at},
        sounds,
    },
    menu::Screen,
};

const RATE: u32 = 48000;
const GUNS: [(ItemKind, &str); 5] =
    [(ItemKind::Ak47, "ak47"), (ItemKind::M16, "m16"), (ItemKind::Mp5, "mp5"), (ItemKind::Uzi, "uzi"), (ItemKind::Pistol, "pistol")];
const NEAR: f32 = 1.0;
const SHORT: f32 = 10.0;
const FAR: f32 = 32.0;
const GLASS_PITCH: (f32, f32) = (0.75, 0.75 + 255.0 / 512.0);

const STEP_MARGIN: f32 = 0.03;
const STEP_SPEED: f32 = 0.5; // how many m/s to count as moving

#[derive(Component, Default)]
struct Stride {
    planted: Option<usize>,
    last_pos: Option<Vec3>,
}

#[derive(Clone)]
struct Clip {
    buffer: SamplesBuffer,
    radius: f32,
}

#[derive(Resource)]
struct GameSounds {
    footsteps: Vec<Clip>,
    guns: HashMap<ItemKind, Vec<Clip>>,
    ricochets: Vec<Clip>,
    body: Vec<Clip>,
    metal: Vec<Clip>,
    glass: Clip,
    explosion: Clip,
    shell: Clip,
    server: HashMap<u8, Clip>,
}

#[derive(Resource, Clone, Copy, Default)]
pub struct Ears {
    pub left: Vec3,
    pub right: Vec3,
    pub volume: f32,
}

pub struct Emitter {
    player: SpatialPlayer,
    at: Vec3,
    scale: f32,
}

impl Emitter {
    pub fn new(mixer: &Mixer, at: Vec3, radius: f32, ears: &Ears) -> Self {
        let scale = radius.sqrt().recip();
        let player = SpatialPlayer::connect_new(mixer, (at * scale).into(), (ears.left * scale).into(), (ears.right * scale).into());

        Self { player, at, scale }
    }

    pub fn player(&self) -> &SpatialPlayer {
        &self.player
    }

    pub fn place(&mut self, at: Vec3, radius: f32, ears: &Ears) {
        (self.at, self.scale) = (at, radius.sqrt().recip());
        self.hear(ears);
    }

    fn hear(&self, ears: &Ears) {
        self.player.set_emitter_position((self.at * self.scale).into());
        self.player.set_left_ear_position((ears.left * self.scale).into());
        self.player.set_right_ear_position((ears.right * self.scale).into());
    }
}

#[derive(Resource, Default)]
struct Playing(Vec<(Emitter, f32)>);

pub struct SoundsPlugin;

impl Plugin for SoundsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Ears>()
            .init_resource::<Playing>()
            .add_systems(Startup, load_sounds)
            .add_systems(Update, (hear, play_sounds, keep_playing).chain().after(Smoothing::View).run_if(in_state(Screen::InGame)))
            .add_systems(Update, footsteps.after(Smoothing::Glide).before(keep_playing).run_if(in_state(Screen::InGame)).run_if(resource_exists::<NetGame>))
            .add_systems(OnExit(Screen::InGame), |mut playing: ResMut<Playing>| playing.0.clear());
    }
}

pub fn decode(path: &Path) -> Option<SamplesBuffer> {
    let bytes = std::fs::read(path).inspect_err(|e| warn!("{}: {e}", path.display())).ok()?;
    let decoder = Decoder::new(Cursor::new(bytes)).inspect_err(|e| warn!("{}: {e}", path.display())).ok()?;
    let (channels, rate) = (decoder.channels(), decoder.sample_rate());

    Some(SamplesBuffer::new(channels, rate, decoder.collect::<Vec<_>>()))
}

fn tone_buffer(samples: Vec<f32>) -> SamplesBuffer {
    SamplesBuffer::new(NonZero::<u16>::MIN, NonZero::new(RATE).expect("a rate"), samples)
}

fn load_sounds(mut commands: Commands) {
    let silence = || tone_buffer(vec![0.0]);
    let file = |name: &str, radius: f32| Clip { buffer: decode(&Path::new("data/sound").join(format!("{name}.wav"))).unwrap_or_else(silence), radius };
    let numbered = |stem: &str, count: usize, radius: f32| (1..=count).map(|k| file(&format!("{stem}{k:02}"), radius)).collect::<Vec<_>>();
    let tone = |key: usize| Clip { buffer: tone_buffer(dial_tone(key)), radius: 0.25 };

    let mut server: HashMap<u8, Clip> = [
        (Sound::CarCrash, file("carcrash02", FAR)),
        (Sound::BodyHit, file("bullethitbody02", SHORT)),
        (Sound::PhoneRing, file("phonering01", NEAR)),
        (Sound::Reload, file("reload01", NEAR)),
        (Sound::GearShift, file("gearshift01", NEAR)),
        (Sound::Modem, file("modem", NEAR)),
        (Sound::Floppy, file("floppy", NEAR)),
    ]
    .into_iter()
    .map(|(sound, clip)| (sound as u8, clip))
    .collect();

    for (key, &sound) in Sound::PHONE_KEYS.iter().enumerate() {
        server.insert(sound as u8, tone(key));
    }

    server.insert(Sound::PhoneBusy as u8, tone(10));

    commands.insert_resource(GameSounds {
        footsteps: numbered("footstep", 4, NEAR),
        guns: GUNS.iter().map(|&(kind, name)| (kind, numbered(&format!("{name}_fire"), 6, SHORT))).collect(),
        ricochets: numbered("ricochet", 8, SHORT),
        body: numbered("bullethitbody", 2, SHORT),
        metal: numbered("bullethitmetal", 2, SHORT),
        glass: file("glassbreak01", SHORT),
        explosion: file("explosion01", FAR),
        shell: file("shell_hit_cement", 2.0),
        server,
    });
}

fn footsteps(
    mut commands: Commands,
    time: Res<Time>,
    net: Res<NetGame>,
    sounds: Option<Res<GameSounds>>,
    speakers: Res<Speakers>,
    ears: Res<Ears>,
    mut playing: ResMut<Playing>,
    mut humans: Query<(Entity, &NetHuman, &Transform, &HumanPose, Option<&mut Stride>)>,
) {
    let (Some(sounds), Some(mixer)) = (sounds, speakers.0.as_ref()) else { return };

    for (e, nh, at, pose, stride) in &mut humans {
        let Some(mut stride) = stride else {
            commands.entity(e).insert(Stride::default());
            continue;
        };
        let Some(h) = net.humans.get(nh.0) else { continue };

        let pos = at.translation;
        let speed = stride.last_pos.map_or(0.0, |p| (pos - p).with_y(0.0).length() / time.delta_secs().max(1e-4));
        stride.last_pos = Some(pos);

        if !h.alive || h.vehicle >= 0 || speed < STEP_SPEED {
            stride.planted = None; // re-arm so the first step after stopping still plays
            continue;
        }

        let feet = [BoneId::FootLeft, BoneId::FootRight].map(|b| bone_at(pos, pose, b.index()).0);
        let lower = match feet[0].y - feet[1].y {
            d if d < -STEP_MARGIN => 0,
            d if d > STEP_MARGIN => 1,
            _ => continue, // inside the hysteresis band: keep the current foot
        };

        if stride.planted.is_some_and(|f| f != lower) {
            if let Some(clip) = pick(&sounds.footsteps) {
                let emitter = Emitter::new(mixer, feet[lower], clip.radius, &ears);
                emitter.player().set_volume(0.5 * ears.volume);
                emitter.player().set_speed(rand::random_range(0.9..1.1));
                emitter.player().append(clip.buffer.clone());
                playing.0.push((emitter, 0.5));
            }
        }
        stride.planted = Some(lower);
    }
}

fn dial_tone(key: usize) -> Vec<f32> {
    let (high, low, length) = match key {
        1 => (1209.0, 697.0, 16384),
        2 => (1336.0, 697.0, 16384),
        3 => (1477.0, 697.0, 16384),
        4 => (1209.0, 770.0, 16384),
        5 => (1336.0, 770.0, 16384),
        6 => (1477.0, 770.0, 16384),
        7 => (1209.0, 852.0, 16384),
        8 => (1336.0, 852.0, 16384),
        9 => (1477.0, 852.0, 16384),
        0 => (1336.0, 941.0, 16384),
        _ => (480.0, 620.0, 22050),
    };
    let step = |f: f64| f * 2.0 * PI / RATE as f64;

    (0..length)
        .map(|i: i32| {
            let fade = 0.9f64.powi(i.saturating_sub(length - 32));
            let t = i as f64;
            (((step(high) * t).sin() + (step(low) * t).sin()) * fade * 0.25) as f32
        })
        .collect()
}

fn hear(camera: Option<Single<&Transform, With<GameCamera>>>, config: Res<Config>, net: Option<Res<NetGame>>, mut ears: ResMut<Ears>) {
    let Some(camera) = camera else { return };
    let side = camera.right() * 0.2 * 0.5;
    let ending = net.filter(|n| n.game_state == GameState::InGame).map_or(1.0, |n| (n.timer as f32 / GAME_FPS).clamp(0.0, 1.0));
    let volume = if config.sound != 0 { config.sound_volume as f32 / 100.0 * ending } else { 0.0 };

    *ears = Ears { left: camera.translation - side, right: camera.translation + side, volume: volume * volume };
}

fn pick(clips: &[Clip]) -> Option<&Clip> {
    clips.get(rand::random_range(0..clips.len().max(1)))
}

fn play_sounds(
    mut happenings: MessageReader<NetEvent>,
    sounds: Option<Res<GameSounds>>,
    speakers: Res<Speakers>,
    ears: Res<Ears>,
    mut playing: ResMut<Playing>,
) {
    let (Some(sounds), Some(mixer)) = (sounds, speakers.0.as_ref()) else { return };

    for happening in happenings.read() {
        let (clip, pos, volume, pitch) = match *happening {
            NetEvent::Gunshot { kind, pos, .. } => (kind.and_then(|k| sounds.guns.get(&k)).and_then(|c| pick(c)), pos, 1.0, 1.0),
            NetEvent::BulletHit { hit, pos, .. } => {
                let clips = match hit {
                    0 => &sounds.ricochets[..],
                    1 => &sounds.body,
                    2 => &sounds.metal,
                    _ => &[],
                };
                (pick(clips), pos, 1.0, 1.0)
            }
            NetEvent::BulletHole { pos, face: 0.., .. } | NetEvent::WindowBroke { pos, .. } => {
                (Some(&sounds.glass), pos, 1.0, rand::random_range(GLASS_PITCH.0..GLASS_PITCH.1))
            }
            NetEvent::Explosion { pos } => (Some(&sounds.explosion), pos, 1.0, 1.0),
            NetEvent::Sound { sound, pos, volume, pitch } => (sounds.server.get(&(sound as u8)), pos, volume, pitch),
            NetEvent::BulletHole { .. } => (None, Vec3::ZERO, 0.0, 1.0),
            NetEvent::ShellHit { pos, volume, pitch } => (Some(&sounds.shell), pos, volume, pitch),
        };

        let Some(clip) = clip else { continue };
        let emitter = Emitter::new(mixer, pos, clip.radius, &ears);

        emitter.player().set_volume(volume.min(1.0) * ears.volume);
        emitter.player().set_speed(pitch);
        emitter.player().append(clip.buffer.clone());
        playing.0.push((emitter, volume.min(1.0)));
    }
}

fn keep_playing(ears: Res<Ears>, mut playing: ResMut<Playing>) {
    playing.0.retain(|(emitter, volume)| {
        emitter.hear(&ears);
        emitter.player().set_volume(volume * ears.volume);
        !emitter.player().empty()
    });
}
