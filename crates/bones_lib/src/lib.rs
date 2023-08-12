//! Opinionated game meta-engine built on Bevy.

#![warn(missing_docs)]
// This cfg_attr is needed because `rustdoc::all` includes lints not supported on stable
#![cfg_attr(doc, allow(unknown_lints))]
#![deny(rustdoc::all)]

#[doc(inline)]
pub use bones_ecs as ecs;

/// Bones lib prelude
pub mod prelude {
    pub use crate::{ecs::prelude::*, Game, Plugin, Session, SessionRunner, Sessions};
}

use std::fmt::Debug;

use bones_asset::AssetServer;

use crate::prelude::*;

/// A bones game. This includes all of the game worlds, and systems.
pub struct Session {
    /// The ECS world for the core.
    pub world: World,
    /// The system
    pub stages: SystemStages,
    /// Whether or not this session should have it's systems run.
    pub active: bool,
    /// Whether or not this session should be rendered.
    pub visible: bool,
    /// The priority of this session relative to other sessions in the [`Game`].
    pub priority: i32,
    /// The session runner to use for this session.
    pub runner: Box<dyn SessionRunner>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("world", &self.world)
            .field("stages", &self.stages)
            .field("active", &self.active)
            .field("visible", &self.visible)
            .field("priority", &self.priority)
            .field("runner", &"SessionRunner")
            .finish()
    }
}

impl Session {
    /// Create an empty [`Session`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Run the session's stages on it's world once.
    pub fn step(&mut self) -> SystemResult {
        self.stages.run(&mut self.world)
    }

    /// Install a plugin.
    pub fn install_plugin(&mut self, plugin: impl Plugin) -> &mut Self {
        plugin.install(self);
        self
    }

    /// Snapshot the world state.
    ///
    /// This is the same as `core.world.clone()`, but it is more explicit.
    pub fn snapshot(&self) -> World {
        self.world.clone()
    }

    /// Restore the world state.
    ///
    /// Re-sets the world state to that of the provided `world`, which may or may not have been
    /// created with [`snapshot()`][Self::snapshot].
    ///
    /// This is the same as doing an [`std::mem::swap`] on `self.world`, but it is more explicit.
    pub fn restore(&mut self, world: &mut World) {
        std::mem::swap(&mut self.world, world)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            world: default(),
            stages: default(),
            active: true,
            visible: true,
            priority: 0,
            runner: Box::new(DefaultSessionRunner),
        }
    }
}

/// Trait for plugins that can be installed into a [`Session`].
pub trait Plugin {
    /// Install the plugin into the [`Session`].
    fn install(self, core: &mut Session);
}
impl<F: FnOnce(&mut Session)> Plugin for F {
    fn install(self, core: &mut Session) {
        (self)(core)
    }
}

/// A session runner is in charge of advancing a [`Session`] simulation.
pub trait SessionRunner: Sync + Send + 'static {
    /// Step the simulation once.
    fn step(&mut self, world: &mut World, stages: &mut SystemStages) -> SystemResult;
}

/// The default [`SessionRunner`], which just runs the systems once every time it is run.
#[derive(Default)]
pub struct DefaultSessionRunner;
impl SessionRunner for DefaultSessionRunner {
    fn step(&mut self, world: &mut World, stages: &mut SystemStages) -> SystemResult {
        stages.run(world)
    }
}

/// The [`Game`] encompasses a complete bones game's logic, independent of the renderer and IO
/// implementations.
///
/// Games are made up of one or more [`Session`]s, each of which contains it's own [`World`] and
/// [`SystemStages`]. These different sessions can be used for parts of the game with independent
/// states, such as the main menu and the gameplay.
#[derive(Debug, Default)]
pub struct Game {
    /// The sessions that make up the game.
    pub sessions: Sessions,
    /// Cache of the session keys, used to sort sessions.
    ///
    /// Not meant for use by the user.
    session_keys_cache: Vec<Key>,
    /// Aditional context provided by game environment before [`Game::step()`] can be called.
    ctx: Option<GameCtx>,
}

impl Game {
    /// Create an empty game.
    pub fn new() -> Self {
        Self::default()
    }

    /// Must be called once before calling [`Game::step()`] to provide context necessary to advance
    /// the game simulation.
    pub fn prepare(&mut self, ctx: GameCtx) {
        self.ctx = Some(ctx);
    }

    /// Returns whether or not [`Game::prepare()`] has been run, and the game is ready to run
    /// [`Game::step()`].
    pub fn has_prepared(&self) -> bool {
        self.ctx.is_some()
    }

    /// Step the game simulation.
    ///
    /// `apply_input` is a function that will be called once for every active [`Session`], allowing
    /// you to update the world with the current frame's input, whatever form that may come in.
    /// Usually this will be used to assign to a resource containing the player's controls and
    /// possibly other information from outside the game such as the window size, etc.
    pub fn step<F: FnMut(&mut World)>(&mut self, mut apply_input: F) {
        let Some(ctx) = &mut self.ctx else {
            panic!("You must call Game::prepare() once before calling Game::step().");
        };

        // Sort session keys by priority
        self.session_keys_cache.clear();
        self.session_keys_cache.extend(self.sessions.map.keys());
        self.session_keys_cache
            .sort_by_key(|name| self.sessions.map.get(name).unwrap().priority);

        // For every session
        for session_name in self.session_keys_cache.drain(..) {
            // Extract the current session
            let mut current_session = self.sessions.map.remove(&session_name).unwrap();

            // If this session is active
            if current_session.active {
                // Apply the game input
                apply_input(&mut current_session.world);

                // Insert the asset server into the session.
                if !current_session.world.resources.contains::<AssetServer>() {
                    current_session
                        .world
                        .resources
                        .insert_cell(ctx.asset_server.clone_cell());
                }

                // Insert the other sessions into the current session's world
                {
                    let mut sessions = current_session.world.resource_mut::<Sessions>();
                    std::mem::swap(&mut *sessions, &mut self.sessions);
                }

                // Step the current session's simulation using it's session runner
                current_session
                    .runner
                    .step(&mut current_session.world, &mut current_session.stages)
                    .unwrap_or_else(|_| panic!("Error running session: {session_name}"));

                // Pull the sessions back out of the world
                {
                    let mut sessions = current_session.world.resource_mut::<Sessions>();
                    std::mem::swap(&mut *sessions, &mut self.sessions);
                }
            }

            // Insert the current session back into the session list
            self.sessions.map.insert(session_name, current_session);
        }
    }
}

/// Extra context provided to the game to prepare it before calling [`Game::step`].
pub struct GameCtx {
    /// A handle to the asset server.
    pub asset_server: AtomicResource<AssetServer>,
}
impl std::fmt::Debug for GameCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameCtx").finish_non_exhaustive()
    }
}

/// Container for multiple game sessions.
///
/// Each session shares the same [`Entities`].
#[derive(HasSchema, Default, Debug)]
#[schema(opaque)]
pub struct Sessions {
    entities: AtomicResource<Entities>,
    map: HashMap<Key, Session>,
}

impl Sessions {
    /// Create a new session, and borrow it mutably so it can be modified.
    #[track_caller]
    pub fn create<K: TryInto<Key>>(&mut self, name: K) -> &mut Session
    where
        <K as TryInto<Key>>::Error: Debug,
    {
        let name = name.try_into().unwrap();
        // Create a blank session
        let mut session = Session::new();

        // Make sure the new session has the same entities as the other sessions.
        session
            .world
            .resources
            .insert_cell(self.entities.clone_cell());

        // Initialize the sessions resource in the session so it will be available in [`Game::step()`].
        session.world.init_resource::<Sessions>();

        // Insert it into the map
        self.map.insert(name, session);

        // And borrow it for the modification
        self.map.get_mut(&name).unwrap()
    }

    /// Delete a session.
    #[track_caller]
    pub fn delete<K: TryInto<Key>>(&mut self, name: K)
    where
        <K as TryInto<Key>>::Error: Debug,
    {
        self.map.remove(&name.try_into().unwrap());
    }

    /// Borrow a session from the sessions list.
    #[track_caller]
    pub fn get<K: TryInto<Key>>(&self, name: K) -> Option<&Session>
    where
        <K as TryInto<Key>>::Error: Debug,
    {
        self.map.get(&name.try_into().unwrap())
    }

    /// Borrow a session from the sessions list.
    #[track_caller]
    pub fn get_mut<K: TryInto<Key>>(&mut self, name: K) -> Option<&mut Session>
    where
        <K as TryInto<Key>>::Error: Debug,
    {
        self.map.get_mut(&name.try_into().unwrap())
    }
}

// We implement `Clone` so that the world can still be snapshot with this resouce in it, but we
// don't actually clone the sessions, since they aren't `Clone`, and the actual sessions shouldn't
// be present in the world when taking a snapshot.
impl Clone for Sessions {
    fn clone(&self) -> Self {
        Self::default()
    }
}