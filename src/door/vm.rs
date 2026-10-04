//! The `.dor` door-animation virtual machine.
//!
//! The door animation is a small bytecode interpreter: 8 command entries, each
//! running one script from the file's script table, plus 12 order (panel) slots
//! whose matrices and vertices the scripts animate. [`Vm::new`] starts entry 0
//! on the main script; the `ACTIVATE` opcode starts the phase scripts on the
//! other entries. [`Vm::step`] runs one 30 Hz frame and returns a [`Frame`]
//! snapshot for the renderer and the transition layer.
//!
//! Faithfulness notes (all derived from the original's behaviour):
//!
//! * `IF_BYTE`/`IF_SHORT` branch when the comparison **fails**.
//! * `LOOP_PUSH` runs its body immediately, then `LOOP` yields once per
//!   iteration.
//! * `ORDER_SETUP` resets the order's local matrix to the exact 4.12 identity
//!   and, when bit `0x80` of the flags high byte is set, clones the referenced
//!   TMD object so `VERT_SET`/`VERT_ADD` writes stay per-order.
//! * The camera is the two points written by `CAM_MATRIX` (from and to), with
//!   focal length `0x101`; the screen is black for the first three frames and
//!   phase 2 holds for five extra frames.
//! * `SFX` pauses dispatch while the caller reports the sound system busy
//!   (`set_sound_busy`); `CLR_FLAGS2` clears that state.

use crate::anim::{self, Mat4x3};
use crate::door::{Dor, ORDER_COUNT, Script};
use crate::model::{Tmd, TmdObject};

/// Number of command entries in the VM.
pub const COMMAND_COUNT: usize = 8;
/// Focal length the door scene renders with (`set_scene_render_param(0x101)`).
pub const FOCAL_LENGTH: i32 = 0x101;
/// Maximum nested `LOOP_PUSH` depth (the original's 8-slot workspace).
const MAX_LOOP_DEPTH: usize = 8;
/// Instruction lengths, indexed by opcode (0x00..=0x25).
const OP_LEN: [u8; 38] = [
    2, 2, 2, 2, 6, 6, 4, 2, 4, 2, 4, 4, 4, 4, 6, 6, // 0x00..=0x0F
    6, 14, 14, 2, 4, 8, 8, 2, 4, 8, 6, 2, 4, 10, 10, 4, // 0x10..=0x1F
    4, 4, 4, 4, 2, 2, // 0x20..=0x25
];

/// The door record fields the scripts can read.
///
/// These latch the player's interaction with the door: `direction` is record
/// byte `+0x08` (which animation arm runs), `sfx` `+0x09`, `door_type` `+0x0A`,
/// `entry_camera` `+0x0B & 0x3F`, and `dest` `+0x0D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DoorParams {
    /// Door direction byte.
    pub direction: u8,
    /// Door sound effect id.
    pub sfx: u8,
    /// Door type byte.
    pub door_type: u8,
    /// Entry camera id (masked to the low six bits by [`Vm::new`]).
    pub entry_camera: u8,
    /// Destination room byte.
    pub dest: u8,
}

/// The scene camera: a from point, a to point and the focal length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorCamera {
    /// Camera position.
    pub from: [i32; 3],
    /// Camera look-at point.
    pub to: [i32; 3],
    /// Focal length in pixels.
    pub focal: i32,
}

impl Default for DoorCamera {
    fn default() -> Self {
        Self {
            from: [0; 3],
            to: [0; 3],
            focal: FOCAL_LENGTH,
        }
    }
}

/// Screen fade state set by `FADE_IN`/`FADE_OUT`.
///
/// `counter` is the per-frame step and `state` the accumulator; the transition
/// layer advances `state` by `counter` each tick and draws the overlay with
/// `alpha = (state >> 7).clamp(0, 255)`. `fade_type` 1 is a white flash, 2 is
/// black.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fade {
    /// Fade type id: 1 white, 2 black.
    pub fade_type: u8,
    /// Per-frame accumulator step.
    pub counter: i32,
    /// Current fade accumulator.
    pub state: i16,
}

/// A sound effect requested by the `SFX` opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sfx {
    /// Sound bank.
    pub bank: u8,
    /// Sound id within the bank.
    pub id: u8,
    /// Volume.
    pub volume: u8,
}

/// A message requested by the `MESSAGE` opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    /// Message id.
    pub id: u8,
    /// Message flags/value.
    pub value: u16,
}

/// One per-vertex write applied to an order's mesh copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexWrite {
    /// `VERT_SET`: the vertex was overwritten.
    Set {
        /// Order slot.
        order: u8,
        /// Vertex index within the order's model.
        vertex: u8,
        /// Written value.
        value: [i16; 3],
    },
    /// `VERT_ADD`: the value was added to the vertex.
    Add {
        /// Order slot.
        order: u8,
        /// Vertex index within the order's model.
        vertex: u8,
        /// Added value.
        value: [i16; 3],
    },
}

/// One order slot in a rendered [`Frame`].
#[derive(Debug)]
pub struct OrderFrame<'a> {
    /// Draw/rotate flags (`0x4000` rotate, `0x8000` draw, low nibble 1/2/3).
    pub flags: u16,
    /// TMD object index this order was set up with.
    pub model: u8,
    /// Parent order for the hierarchical matrix.
    pub parent: Option<u8>,
    /// Texture page byte written by `ORDER_TPAGE`.
    pub tpage: u8,
    /// Local matrix: 4.12 rotation (rebuilt from `rotation` when the rotate
    /// flag is set) and integer translation.
    pub local: Mat4x3,
    /// Composed world matrix (parent chain applied, camera not applied).
    pub matrix: Mat4x3,
    /// Current 12-bit Euler rotation.
    pub rotation: [i16; 3],
    /// Current position velocity.
    pub velocity: [i16; 3],
    /// Current rotation velocity.
    pub rot_velocity: [i16; 3],
    /// This order's mesh copy, present once `ORDER_SETUP` created it.
    pub mesh: Option<&'a TmdObject>,
    /// Vertex writes applied during this frame.
    pub vertex_writes: Vec<VertexWrite>,
}

/// One rendered frame of the door animation.
///
/// Borrows the VM for the per-order mesh copies; render it before stepping
/// again.
#[derive(Debug)]
pub struct Frame<'a> {
    /// Scene camera.
    pub camera: DoorCamera,
    /// Current fade state.
    pub fade: Fade,
    /// The 12 order slots.
    pub orders: Vec<OrderFrame<'a>>,
    /// Sound effects emitted this frame.
    pub sfx: Vec<Sfx>,
    /// Messages emitted this frame.
    pub messages: Vec<Message>,
    /// The animation has ended.
    pub done: bool,
    /// Phase used for this frame.
    pub phase: i16,
    /// Frame counter used for this frame.
    pub frame: u32,
    /// Hold counter used for this frame.
    pub hold: u8,
    /// The original draws the screen black for the first three frames.
    pub black: bool,
}

/// A script cursor: which script and byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    script: usize,
    pc: usize,
}

/// One command entry's state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Command {
    active: bool,
    depth: usize,
    cursor: Option<Cursor>,
    ret: [usize; MAX_LOOP_DEPTH],
    loops: [u16; MAX_LOOP_DEPTH],
}

impl Default for Command {
    fn default() -> Self {
        Self {
            active: false,
            depth: 0,
            cursor: None,
            ret: [0; MAX_LOOP_DEPTH],
            loops: [0; MAX_LOOP_DEPTH],
        }
    }
}

/// One order slot's live state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderState {
    flags: u16,
    model: u8,
    parent: Option<u8>,
    tpage: u8,
    local: Mat4x3,
    rotation: [i16; 3],
    velocity: [i16; 3],
    rot_velocity: [i16; 3],
    mesh: Option<TmdObject>,
    writes: Vec<VertexWrite>,
}

impl Default for OrderState {
    fn default() -> Self {
        Self {
            flags: 0,
            model: 0,
            parent: None,
            tpage: 0,
            local: identity(),
            rotation: [0; 3],
            velocity: [0; 3],
            rot_velocity: [0; 3],
            mesh: None,
            writes: Vec::new(),
        }
    }
}

/// The exact 4.12 identity the original's `InitScaMatrix` writes.
fn identity() -> Mat4x3 {
    Mat4x3 {
        r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
        t: [0, 0, 0],
    }
}

/// The door-animation interpreter.
///
/// [`Vm::new`] clones the script table and the base mesh out of the [`Dor`],
/// so the VM is self-contained and can be stored next to (or without) the
/// parsed file.
#[derive(Debug, Clone)]
pub struct Vm {
    scripts: Vec<Script>,
    mesh: Tmd,
    params: DoorParams,
    commands: [Command; COMMAND_COUNT],
    orders: [OrderState; ORDER_COUNT],
    camera: DoorCamera,
    delta: [i16; 6],
    short_var: i16,
    scratch: u8,
    message_var: u8,
    state: u8,
    sound_busy: bool,
    frame: u32,
    phase: i16,
    hold: u8,
    fade: Fade,
    sfx: Vec<Sfx>,
    messages: Vec<Message>,
}

impl Vm {
    /// Start the animation on the main script (entry 0) with the given door
    /// record fields.
    ///
    /// `params.entry_camera` is masked to six bits, matching the original's
    /// latch of record byte `+0x0B`.
    pub fn new(dor: &Dor, mut params: DoorParams) -> Self {
        params.entry_camera &= 0x3F;
        let mut commands: [Command; COMMAND_COUNT] = std::array::from_fn(|_| Command::default());
        commands[0].active = true;
        commands[0].cursor = if dor.scripts.is_empty() {
            None
        } else {
            Some(Cursor { script: 0, pc: 0 })
        };
        Self {
            scripts: dor.scripts.clone(),
            mesh: dor.mesh.clone(),
            params,
            commands,
            orders: std::array::from_fn(|_| OrderState::default()),
            camera: DoorCamera::default(),
            delta: [0; 6],
            short_var: 0,
            scratch: 0,
            message_var: 0,
            state: 3,
            sound_busy: false,
            frame: 0,
            phase: 0,
            hold: 0,
            fade: Fade::default(),
            sfx: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Run one 30 Hz frame and return its render snapshot.
    ///
    /// When the animation has already ended the frame is returned unchanged
    /// (`done` set) and the phase/frame counters do not advance.
    pub fn step(&mut self) -> Frame<'_> {
        if self.state & 1 == 0 {
            return self.snapshot(true, self.frame, self.phase, self.hold);
        }

        if self.state & 2 != 0 {
            for index in 0..COMMAND_COUNT {
                if self.commands[index].active && self.commands[index].cursor.is_some() {
                    self.run_entry(index);
                }
            }
        } else if !self.sound_busy {
            self.state |= 2;
        }

        for order in &mut self.orders {
            if order.flags & 0x4000 != 0 {
                order.local.r = anim::rotation_matrix(
                    i32::from(order.rotation[0]),
                    i32::from(order.rotation[1]),
                    i32::from(order.rotation[2]),
                );
            }
        }

        let done = self.state & 1 == 0;
        let frame = self.frame;
        let phase = self.phase;
        let hold = self.hold;
        if self.phase == 2 && self.hold < 5 {
            self.hold += 1;
        } else {
            self.hold = 0;
            self.phase += 1;
            self.frame += 1;
        }
        self.snapshot(done, frame, phase, hold)
    }

    /// End the animation immediately (the original's held-input skip).
    pub fn finish(&mut self) {
        self.state = 0;
    }

    /// Whether the animation has ended.
    pub fn is_done(&self) -> bool {
        self.state & 1 == 0
    }

    /// Whether command dispatch is currently enabled.
    pub fn dispatch_enabled(&self) -> bool {
        self.state & 2 != 0
    }

    /// Whether the caller has marked the sound system busy. While busy the
    /// `SFX` opcode pauses dispatch and the `CLR_FLAGS2` opcode clears it.
    pub fn set_sound_busy(&mut self, busy: bool) {
        self.sound_busy = busy;
    }

    /// The current sound-busy state.
    pub fn sound_busy(&self) -> bool {
        self.sound_busy
    }

    /// Whether command entry `index` is active.
    pub fn command_active(&self, index: usize) -> bool {
        self.commands.get(index).is_some_and(|entry| entry.active)
    }

    /// The current value of a script byte variable.
    ///
    /// `0` direction, `1` sfx, `2` door type, `3` entry camera, `4` dest,
    /// `5` scratch, `6` message. Out-of-range indices read variable 0, exactly
    /// like the original's `IF_BYTE` fallback.
    pub fn byte_var(&self, index: u8) -> u8 {
        match index {
            1 => self.params.sfx,
            2 => self.params.door_type,
            3 => self.params.entry_camera,
            4 => self.params.dest,
            5 => self.scratch,
            6 => self.message_var,
            _ => self.params.direction,
        }
    }

    /// The single short script variable.
    pub fn short_var(&self) -> i16 {
        self.short_var
    }

    /// The door record fields the VM was started with.
    pub fn params(&self) -> DoorParams {
        self.params
    }

    fn snapshot(&mut self, done: bool, frame: u32, phase: i16, hold: u8) -> Frame<'_> {
        let worlds = compose_worlds(&self.orders);
        let mut orders = Vec::with_capacity(ORDER_COUNT);
        for (index, state) in self.orders.iter_mut().enumerate() {
            orders.push(OrderFrame {
                flags: state.flags,
                model: state.model,
                parent: state.parent,
                tpage: state.tpage,
                local: state.local,
                matrix: worlds[index],
                rotation: state.rotation,
                velocity: state.velocity,
                rot_velocity: state.rot_velocity,
                mesh: state.mesh.as_ref(),
                vertex_writes: std::mem::take(&mut state.writes),
            });
        }
        Frame {
            camera: self.camera,
            fade: self.fade,
            orders,
            sfx: std::mem::take(&mut self.sfx),
            messages: std::mem::take(&mut self.messages),
            done,
            phase,
            frame,
            hold,
            black: frame < 3,
        }
    }

    /// Execute one command entry until an opcode yields or the script ends.
    fn run_entry(&mut self, index: usize) {
        loop {
            let Some(cursor) = self.commands[index].cursor else {
                return;
            };
            let Some(opcode) = self.script_byte(cursor) else {
                self.clear_command(index);
                return;
            };
            let length = usize::from(OP_LEN.get(usize::from(opcode)).copied().unwrap_or(0));
            if length == 0 {
                self.clear_command(index);
                return;
            }
            let Some(instruction) = self.instruction(cursor, length) else {
                self.clear_command(index);
                return;
            };
            if !self.exec(index, cursor, &instruction) {
                return;
            }
        }
    }

    fn script_byte(&self, cursor: Cursor) -> Option<u8> {
        self.scripts
            .get(cursor.script)?
            .bytes
            .get(cursor.pc)
            .copied()
    }

    fn instruction(&self, cursor: Cursor, length: usize) -> Option<[u8; 14]> {
        let bytes = self
            .scripts
            .get(cursor.script)?
            .bytes
            .get(cursor.pc..cursor.pc + length)?;
        let mut instruction = [0u8; 14];
        instruction[..length].copy_from_slice(bytes);
        Some(instruction)
    }

    fn set_pc(&mut self, index: usize, script: usize, pc: usize) {
        self.commands[index].cursor = Some(Cursor { script, pc });
    }

    fn clear_command(&mut self, index: usize) {
        self.commands[index] = Command::default();
    }

    /// Execute one instruction; returns `true` to keep dispatching.
    fn exec(&mut self, index: usize, cursor: Cursor, op: &[u8; 14]) -> bool {
        let (script, pc) = (cursor.script, cursor.pc);
        match op[0] {
            0x00 => {
                self.state = 0;
                false
            }
            0x01 => {
                self.clear_command(index);
                false
            }
            0x02 => {
                let target = usize::from(op[1]);
                if self.commands.get(target).is_some_and(|entry| entry.active) {
                    false
                } else {
                    self.set_pc(index, script, pc + 2);
                    true
                }
            }
            0x03 => {
                self.set_pc(index, script, pc + 2);
                false
            }
            0x04 => {
                let variable = i32::from(self.byte_var(op[2]) as i8);
                let taken = branch_taken(variable, op[3], i32::from(op[4] as i8));
                self.set_pc(
                    index,
                    script,
                    if taken {
                        pc + 6 + usize::from(op[1])
                    } else {
                        pc + 6
                    },
                );
                true
            }
            0x05 => {
                let value = i16_at(op, 4);
                let taken = branch_taken(i32::from(self.short_var), op[3], i32::from(value));
                self.set_pc(
                    index,
                    script,
                    if taken {
                        pc + 6 + usize::from(op[1])
                    } else {
                        pc + 6
                    },
                );
                true
            }
            0x06 => {
                let depth = self.commands[index].depth;
                if depth < MAX_LOOP_DEPTH {
                    self.commands[index].ret[depth] = pc + 4;
                    self.commands[index].loops[depth] = u16_at(op, 2);
                    self.commands[index].depth = depth + 1;
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x07 => {
                let depth = self.commands[index].depth;
                if depth == 0 {
                    self.set_pc(index, script, pc + 2);
                    return true;
                }
                let slot = depth - 1;
                let counter = self.commands[index].loops[slot].wrapping_sub(1);
                self.commands[index].loops[slot] = counter;
                if counter < 1 {
                    self.commands[index].depth = slot;
                    self.set_pc(index, script, pc + 2);
                    true
                } else {
                    let target = self.commands[index].ret[slot];
                    self.set_pc(index, script, target);
                    false
                }
            }
            0x08 => {
                let word = u16_at(op, 2);
                let target = usize::from(word & 0xFF);
                let script_index = usize::from((word >> 6) & !3) / 4;
                if target < COMMAND_COUNT && script_index < self.scripts.len() {
                    let command = &mut self.commands[target];
                    *command = Command::default();
                    command.active = true;
                    command.cursor = Some(Cursor {
                        script: script_index,
                        pc: 0,
                    });
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x09 => {
                let target = usize::from(op[1]);
                if target < COMMAND_COUNT {
                    self.clear_command(target);
                }
                self.set_pc(index, script, pc + 2);
                true
            }
            0x0A => {
                self.set_byte_var(op[1], op[2]);
                self.set_pc(index, script, pc + 4);
                true
            }
            0x0B => {
                self.add_byte_var(op[1], op[2] as i8);
                self.set_pc(index, script, pc + 4);
                true
            }
            0x0C => {
                self.short_var = i16_at(op, 2);
                self.set_pc(index, script, pc + 4);
                true
            }
            0x0D => {
                self.short_var = self.short_var.wrapping_add(i16_at(op, 2));
                self.set_pc(index, script, pc + 4);
                true
            }
            0x0E => {
                self.fade.fade_type = op[1];
                self.fade.counter = i32::from(i16_at(op, 2)) * -0x80;
                self.fade.state = i16_at(op, 4);
                self.set_pc(index, script, pc + 6);
                true
            }
            0x0F => {
                self.fade.fade_type = op[1];
                self.fade.counter = i32::from(i16_at(op, 2)) << 7;
                self.fade.state = i16_at(op, 4);
                self.set_pc(index, script, pc + 6);
                true
            }
            0x10 => {
                let order = usize::from(op[1]);
                let flags = u16_at(op, 4);
                if let Some(state) = self.orders.get_mut(order) {
                    state.flags = flags;
                    state.model = op[2];
                    state.parent = (op[3] != 0xFF).then_some(op[3]);
                    state.local = identity();
                    if flags & 0x8000 != 0 {
                        state.mesh = self.mesh.objects.get(usize::from(op[2])).cloned();
                    }
                }
                self.set_pc(index, script, pc + 6);
                true
            }
            0x11 => {
                self.camera.focal = FOCAL_LENGTH;
                let shift = u32::from(op[1] & 0x1F);
                for axis in 0..3 {
                    self.camera.from[axis] = i32::from(i16_at(op, 2 + axis * 2)) << shift;
                    self.camera.to[axis] = i32::from(i16_at(op, 8 + axis * 2)) << shift;
                }
                self.set_pc(index, script, pc + 14);
                true
            }
            0x12 => {
                for (slot, value) in self.delta.iter_mut().enumerate() {
                    *value = i16_at(op, 2 + slot * 2);
                }
                self.set_pc(index, script, pc + 14);
                true
            }
            0x13 => {
                for slot in 0..3 {
                    self.camera.from[slot] =
                        self.camera.from[slot].wrapping_add(i32::from(self.delta[slot]));
                    self.camera.to[slot] =
                        self.camera.to[slot].wrapping_add(i32::from(self.delta[3 + slot]));
                }
                self.set_pc(index, script, pc + 2);
                true
            }
            0x14 => {
                let slot = usize::from(op[2]);
                if let Some(delta) = self.delta.get_mut(slot) {
                    *delta = delta.wrapping_add(i16::from(op[3] as i8));
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x15 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    state.local.t = [
                        i32::from(i16_at(op, 2)),
                        i32::from(i16_at(op, 4)),
                        i32::from(i16_at(op, 6)),
                    ];
                }
                self.set_pc(index, script, pc + 8);
                true
            }
            0x16 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    state.velocity = [i16_at(op, 2), i16_at(op, 4), i16_at(op, 6)];
                }
                self.set_pc(index, script, pc + 8);
                true
            }
            0x17 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    for (position, velocity) in state.local.t.iter_mut().zip(state.velocity) {
                        *position = position.wrapping_add(i32::from(velocity));
                    }
                }
                self.set_pc(index, script, pc + 2);
                true
            }
            0x18 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    let slot = usize::from(op[2]);
                    if let Some(velocity) = state.velocity.get_mut(slot) {
                        *velocity = velocity.wrapping_add(i16::from(op[3] as i8));
                    }
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x19 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    state.rotation = [i16_at(op, 2), i16_at(op, 4), i16_at(op, 6)];
                }
                self.set_pc(index, script, pc + 8);
                true
            }
            0x1A => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    state.rot_velocity = [
                        i16::from(op[2] as i8),
                        i16::from(op[3] as i8),
                        i16::from(op[4] as i8),
                    ];
                }
                self.set_pc(index, script, pc + 6);
                true
            }
            0x1B => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    for (rotation, velocity) in state.rotation.iter_mut().zip(state.rot_velocity) {
                        *rotation = rotation.wrapping_add(velocity);
                    }
                }
                self.set_pc(index, script, pc + 2);
                true
            }
            0x1C => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1])) {
                    let slot = usize::from(op[2]);
                    if let Some(velocity) = state.rot_velocity.get_mut(slot) {
                        *velocity = velocity.wrapping_add(i16::from(op[3] as i8));
                    }
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x1D => {
                self.write_vertex(op, true);
                self.set_pc(index, script, pc + 10);
                true
            }
            0x1E => {
                self.write_vertex(op, false);
                self.set_pc(index, script, pc + 10);
                true
            }
            0x1F => {
                self.messages.push(Message {
                    id: op[1],
                    value: u16_at(op, 2),
                });
                self.set_pc(index, script, pc + 4);
                true
            }
            0x20 => {
                if self.sound_busy {
                    self.state &= !2;
                    return false;
                }
                self.sfx.push(Sfx {
                    bank: op[1],
                    id: op[2],
                    volume: op[3],
                });
                self.set_pc(index, script, pc + 4);
                true
            }
            0x21 => {
                if let Some(state) = self.orders.get_mut(usize::from(op[1]))
                    && state.flags != 0
                {
                    state.flags = u16_at(op, 2);
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x22 => {
                let word = u16_at(op, 2);
                if let Some(state) = self.orders.get_mut(usize::from(word & 0xFF)) {
                    state.tpage = (word >> 8) as u8;
                }
                self.set_pc(index, script, pc + 4);
                true
            }
            0x23 => {
                let cont = u16_at(op, 2) >> 1;
                self.set_pc(index, script, pc + 4);
                cont != 0
            }
            0x24 => {
                self.state &= !2;
                self.set_pc(index, script, pc + 2);
                false
            }
            0x25 => {
                self.sound_busy = false;
                self.set_pc(index, script, pc + 2);
                true
            }
            _ => {
                self.clear_command(index);
                false
            }
        }
    }

    /// Apply a `VERT_SET`/`VERT_ADD` write to the order's own mesh copy.
    ///
    /// `VERT_ADD` is the one the shipped data uses (ELE03's cage doors nudge
    /// individual vertices); `VERT_SET` is kept for completeness. The original
    /// resolves each order to a private TMD copy at `ORDER_SETUP`, so writes
    /// never leak between panels.
    fn write_vertex(&mut self, op: &[u8; 14], set: bool) {
        let order = usize::from(op[1]);
        let vertex = usize::from(op[2]);
        let value = [i16_at(op, 4), i16_at(op, 6), i16_at(op, 8)];
        if let Some(state) = self.orders.get_mut(order)
            && let Some(object) = state.mesh.as_mut()
            && let Some(target) = object.vertices.get_mut(vertex)
        {
            if set {
                *target = value;
                state.writes.push(VertexWrite::Set {
                    order: op[1],
                    vertex: op[2],
                    value,
                });
            } else {
                for (channel, delta) in target.iter_mut().zip(value) {
                    *channel = channel.wrapping_add(delta);
                }
                state.writes.push(VertexWrite::Add {
                    order: op[1],
                    vertex: op[2],
                    value,
                });
            }
        }
    }

    fn set_byte_var(&mut self, index: u8, value: u8) {
        match index {
            0 => self.params.direction = value,
            1 => self.params.sfx = value,
            2 => self.params.door_type = value,
            3 => self.params.entry_camera = value,
            4 => self.params.dest = value,
            5 => self.scratch = value,
            6 => self.message_var = value,
            _ => {}
        }
    }

    fn add_byte_var(&mut self, index: u8, delta: i8) {
        let value = self.byte_var(index).wrapping_add(delta as u8);
        self.set_byte_var(index, value);
    }
}

/// Branch condition for the compare opcodes: the branch is taken when the
/// comparison FAILS. Relation 6 and above is an unconditional jump.
fn branch_taken(variable: i32, relation: u8, value: i32) -> bool {
    match relation {
        0 => variable != value,
        1 => variable <= value,
        2 => variable < value,
        3 => variable >= value,
        4 => variable > value,
        5 => variable == value,
        _ => true,
    }
}

/// Compose every order's local matrix through its parent chain.
///
/// Cycles and out-of-range parents are treated as roots, matching the depth
/// guard the original's chain walker applies.
fn compose_worlds(orders: &[OrderState; ORDER_COUNT]) -> [Mat4x3; ORDER_COUNT] {
    let mut world: [Option<Mat4x3>; ORDER_COUNT] = [None; ORDER_COUNT];
    for start in 0..ORDER_COUNT {
        if world[start].is_some() {
            continue;
        }
        let mut chain = Vec::new();
        let mut seen = [false; ORDER_COUNT];
        let mut current = start;
        loop {
            if world[current].is_some() {
                break;
            }
            if seen[current] {
                world[current] = Some(orders[current].local);
                break;
            }
            seen[current] = true;
            chain.push(current);
            match orders[current].parent {
                Some(parent)
                    if usize::from(parent) < ORDER_COUNT && usize::from(parent) != current =>
                {
                    current = usize::from(parent);
                }
                _ => {
                    world[current] = Some(orders[current].local);
                    break;
                }
            }
        }
        for &index in chain.iter().rev() {
            if world[index].is_some() {
                continue;
            }
            let local = orders[index].local;
            let parent = orders[index]
                .parent
                .map(usize::from)
                .filter(|&parent| parent < ORDER_COUNT && parent != index)
                .and_then(|parent| world[parent]);
            world[index] = Some(match parent {
                Some(parent) => anim::compose(&parent, &local),
                None => local,
            });
        }
    }
    world.map(|matrix| matrix.expect("every order resolves"))
}

fn i16_at(op: &[u8; 14], offset: usize) -> i16 {
    i16::from_le_bytes([op[offset], op[offset + 1]])
}

fn u16_at(op: &[u8; 14], offset: usize) -> u16 {
    u16::from_le_bytes([op[offset], op[offset + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Texture8;

    fn dor_with(scripts: Vec<Vec<u8>>) -> Dor {
        let scripts = scripts
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| Script {
                offset: index,
                bytes,
            })
            .collect();
        Dor {
            scripts,
            mesh: Tmd {
                objects: vec![TmdObject {
                    vertices: vec![[0, 0, 0]; 8],
                    normals: vec![[0, 0, 0]],
                    prims: Vec::new(),
                }],
            },
            texture: Texture8 {
                width: 1,
                height: 1,
                indices: vec![0],
                palettes: vec![[0, 0, 0, 255]],
            },
            orders: [crate::door::Order::default(); ORDER_COUNT],
        }
    }

    fn run(dor: &Dor, params: DoorParams) -> Vm {
        Vm::new(dor, params)
    }

    #[test]
    fn end_stops_the_animation_and_freezes_the_counters() {
        let dor = dor_with(vec![vec![0x00, 0x00]]);
        let mut vm = run(&dor, DoorParams::default());

        {
            let frame = vm.step();
            assert!(frame.done);
            assert_eq!(frame.frame, 0);
        }
        assert!(vm.is_done());

        // The END frame still advances the counters once (the original's loop
        // increments before re-testing its running flag), then freezes.
        let frame = vm.step();
        assert!(frame.done);
        assert_eq!(frame.frame, 1, "ended animation must not advance again");
    }

    #[test]
    fn clear_self_deactivates_the_entry() {
        let dor = dor_with(vec![vec![0x01, 0x00, 0x0A, 0x05, 0x01, 0x00]]);
        let mut vm = run(&dor, DoorParams::default());

        vm.step();

        assert!(!vm.command_active(0));
        assert!(!vm.is_done());
        assert_eq!(vm.byte_var(5), 0, "bytes after CLEAR_SELF must not run");
    }

    #[test]
    fn wait_free_blocks_until_the_target_entry_clears() {
        // Entry 0: activate entry 1 (script 1), wait for it, set var 5 = 7.
        let entry0 = vec![
            0x08, 0x00, 0x01, 0x01, 0x02, 0x01, 0x0A, 0x05, 0x07, 0x00, 0x01, 0x00,
        ];
        let entry1 = vec![0x01, 0x00];
        let dor = dor_with(vec![entry0, entry1]);
        let mut vm = run(&dor, DoorParams::default());

        vm.step();
        assert_eq!(vm.byte_var(5), 0, "WAIT_FREE must yield while entry 1 runs");

        vm.step();
        assert_eq!(vm.byte_var(5), 7);
    }

    #[test]
    fn yield_resumes_on_the_next_step() {
        let dor = dor_with(vec![vec![0x03, 0x00, 0x0A, 0x05, 0x01, 0x00, 0x01, 0x00]]);
        let mut vm = run(&dor, DoorParams::default());

        vm.step();
        assert_eq!(vm.byte_var(5), 0);

        vm.step();
        assert_eq!(vm.byte_var(5), 1);
    }

    #[test]
    fn if_byte_branches_when_the_comparison_fails() {
        // IF_BYTE var0 == 3 -> skip to the +6 arm; else fall through.
        let script = vec![
            0x04, 0x06, 0x00, 0x00, 0x03, 0x00, // if byteVar0 == 3
            0x0A, 0x05, 0x01, 0x00, // var5 = 1
            0x01, 0x00, // clear self
            0x0A, 0x05, 0x02, 0x00, // var5 = 2
            0x01, 0x00, // clear self
        ];

        let dor = dor_with(vec![script.clone()]);
        let mut vm = run(
            &dor,
            DoorParams {
                direction: 3,
                ..DoorParams::default()
            },
        );
        vm.step();
        assert_eq!(vm.byte_var(5), 1, "equality must fall through");

        let mut vm = run(
            &dor,
            DoorParams {
                direction: 1,
                ..DoorParams::default()
            },
        );
        vm.step();
        assert_eq!(vm.byte_var(5), 2, "inequality must branch");
    }

    #[test]
    fn if_short_compares_and_unconditionally_branches() {
        // SSET 5; IF_SHORT var == 5 -> fall through (var5=1) else branch (var5=2).
        let script = vec![
            0x0C, 0x00, 0x05, 0x00, // shortVar = 5
            0x05, 0x06, 0x00, 0x00, 0x05, 0x00, // if shortVar == 5
            0x0A, 0x05, 0x01, 0x00, 0x01, 0x00, // var5 = 1
            0x0A, 0x05, 0x02, 0x00, 0x01, 0x00, // var5 = 2
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.short_var(), 5);
        assert_eq!(vm.byte_var(5), 1, "equality must fall through");

        // relop 6 is an unconditional branch.
        let script = vec![
            0x05, 0x06, 0x00, 0x06, 0x00, 0x00, // if shortVar (unconditional)
            0x0A, 0x05, 0x01, 0x00, 0x01, 0x00, // var5 = 1
            0x0A, 0x05, 0x02, 0x00, 0x01, 0x00, // var5 = 2
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 2);
    }

    #[test]
    fn loop_push_runs_the_body_then_yields_each_iteration() {
        // LOOP_PUSH 3; BADD var5 +1; LOOP
        let script = vec![0x06, 0x00, 0x03, 0x00, 0x0B, 0x05, 0x01, 0x00, 0x07, 0x00];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());

        vm.step();
        assert_eq!(vm.byte_var(5), 1);
        vm.step();
        assert_eq!(vm.byte_var(5), 2);
        vm.step();
        assert_eq!(vm.byte_var(5), 3);
        vm.step();
        assert_eq!(vm.byte_var(5), 3);
        assert!(!vm.command_active(0), "the loop must finish");
    }

    #[test]
    fn activate_and_clear_entry_target_other_commands() {
        // Entry 0: activate entry 2 on script 1, then clear it before it runs.
        let entry0 = vec![
            0x08, 0x00, 0x02, 0x01, 0x09, 0x02, 0x0A, 0x05, 0x01, 0x00, 0x01, 0x00,
        ];
        let entry1 = vec![0x0A, 0x05, 0x09, 0x00, 0x01, 0x00];
        let dor = dor_with(vec![entry0, entry1.clone()]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 1);
        assert!(!vm.command_active(2));

        // Without CLEAR_ENTRY the activated entry runs in the same dispatch.
        let entry0 = vec![0x08, 0x00, 0x02, 0x01, 0x01, 0x00];
        let dor = dor_with(vec![entry0, entry1.clone()]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 9);
    }

    #[test]
    fn byte_and_short_arithmetic_wraps_like_the_original() {
        let script = vec![
            0x0A, 0x05, 0x0A, 0x00, // var5 = 10
            0x0B, 0x05, 0xFB, 0x00, // var5 += -5
            0x0C, 0x00, 0x00, 0x01, // shortVar = 0x0100
            0x0D, 0x00, 0xFE, 0xFF, // shortVar += -2
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 5);
        assert_eq!(vm.short_var(), 254);
    }

    #[test]
    fn fade_ops_set_type_counter_and_state() {
        let script = vec![
            0x0F, 0x02, 0x0A, 0x00, 0x02, 0x00, // FADE_OUT type 2, 10, state 2
            0x03, 0x00, // YIELD
            0x0E, 0x01, 0x08, 0x00, 0x00, 0x22, // FADE_IN type 1, 8, state 0x2200
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());

        let frame = vm.step();
        assert_eq!(
            frame.fade,
            Fade {
                fade_type: 2,
                counter: 1280,
                state: 2
            }
        );

        let frame = vm.step();
        assert_eq!(
            frame.fade,
            Fade {
                fade_type: 1,
                counter: -1024,
                state: 0x2200
            }
        );
    }

    #[test]
    fn camera_matrix_deltas_move_the_scene_camera() {
        let script = vec![
            0x11, 0x01, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06,
            0x00, // CAM_MATRIX shift 1
            0x12, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06,
            0x00, // DELTA_LOAD
            0x13, 0x00, // MAT_ADD_DELTA
            0x14, 0x00, 0x00, 0x01, // DELTA_ADD delta[0] += 1
            0x13, 0x00, // MAT_ADD_DELTA
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        let frame = vm.step();

        // from += (2,2,3) on the second add, to += (4,5,6).
        assert_eq!(frame.camera.from, [5, 8, 12]);
        assert_eq!(frame.camera.to, [16, 20, 24]);
        assert_eq!(frame.camera.focal, FOCAL_LENGTH);
    }

    #[test]
    fn order_setup_positions_rotates_and_flags() {
        let script = vec![
            0x10, 0x00, 0x00, 0xFF, 0x01, 0xC0, // ORDER_SETUP 0 model 0 no parent C001
            0x15, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, // ORDER_POS (1,2,3)
            0x16, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E, 0x00, // ORDER_VEL (10,20,30)
            0x17, 0x00, // POS += VEL
            0x18, 0x00, 0x00, 0x05, // VEL_ADD 0 += 5
            0x19, 0x00, 0x04, 0x00, 0x05, 0x00, 0x06, 0x00, // ORDER_ROT (4,5,6)
            0x1A, 0x00, 0x01, 0x02, 0x03, 0x00, // ORDER_ROTVEL (1,2,3)
            0x1B, 0x00, // ROT += RVEL
            0x1C, 0x00, 0x00, 0x01, // ROTVEL_ADD 0 += 1
            0x21, 0x00, 0x03, 0xC0, // ORDER_FLAGS C003
            0x22, 0x00, 0x00, 0x40, // ORDER_TPAGE order 0 tpage 0x40
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        let frame = vm.step();

        let order = &frame.orders[0];
        assert_eq!(order.flags, 0xC003);
        assert_eq!(order.local.t, [11, 22, 33]);
        assert_eq!(order.velocity, [15, 20, 30]);
        assert_eq!(order.rotation, [5, 7, 9]);
        assert_eq!(order.rot_velocity, [2, 2, 3]);
        assert_eq!(order.tpage, 0x40);
        assert!(order.mesh.is_some());
        assert_eq!(
            order.matrix, order.local,
            "a root order's world is its local"
        );
    }

    #[test]
    fn order_hierarchy_composes_parent_matrices() {
        let script = vec![
            0x10, 0x00, 0x00, 0xFF, 0x00, 0x80, // ORDER_SETUP 0 root, draw only
            0x10, 0x01, 0x00, 0x00, 0x00, 0x80, // ORDER_SETUP 1 parent 0, draw only
            0x15, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, // ORDER_POS 0 (0,0,100)
            0x15, 0x01, 0x00, 0x00, 0x00, 0x00, 0x38, 0xFF, // ORDER_POS 1 (0,0,-200)
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        let frame = vm.step();

        assert_eq!(frame.orders[0].matrix.t, [0, 0, 100]);
        assert_eq!(frame.orders[1].matrix.t, [0, 0, -100]);
    }

    #[test]
    fn vert_set_and_add_write_the_order_mesh_copy() {
        let script = vec![
            0x10, 0x00, 0x00, 0xFF, 0x00, 0x80, // ORDER_SETUP 0 model 0, draw only
            0x1D, 0x00, 0x01, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E,
            0x00, // VERT_SET v1 (10,20,30)
            0x1E, 0x00, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, // VERT_ADD v1 (1,2,3)
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        let frame = vm.step();

        let order = &frame.orders[0];
        assert_eq!(order.mesh.expect("mesh copy").vertices[1], [11, 22, 33]);
        assert_eq!(order.vertex_writes.len(), 2);
        assert_eq!(
            order.vertex_writes[0],
            VertexWrite::Set {
                order: 0,
                vertex: 1,
                value: [10, 20, 30]
            }
        );
        assert_eq!(
            order.vertex_writes[1],
            VertexWrite::Add {
                order: 0,
                vertex: 1,
                value: [1, 2, 3]
            }
        );
    }

    #[test]
    fn message_and_sfx_are_emitted() {
        let script = vec![
            0x1F, 0x5A, 0xFF, 0x00, // MESSAGE 0x5A, 0x00FF
            0x20, 0x00, 0x01, 0x00, // SFX 0, 1, 0
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        let frame = vm.step();

        assert_eq!(
            frame.messages,
            [Message {
                id: 0x5A,
                value: 0xFF
            }]
        );
        assert_eq!(
            frame.sfx,
            [Sfx {
                bank: 0,
                id: 1,
                volume: 0
            }]
        );
    }

    #[test]
    fn buffer_advance_continues_when_nonzero_and_yields_on_zero() {
        let script = vec![
            0x23, 0x00, 0x00, 0x20, // BUF_ADV 0x2000
            0x0A, 0x05, 0x01, 0x00, 0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 1);

        let script = vec![
            0x23, 0x00, 0x00, 0x00, // BUF_ADV 0
            0x0A, 0x05, 0x01, 0x00, 0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        assert_eq!(vm.byte_var(5), 0, "a zero advance yields");
        vm.step();
        assert_eq!(vm.byte_var(5), 1);
    }

    #[test]
    fn disable_dispatch_pauses_exactly_one_frame() {
        let script = vec![
            0x24, 0x00, // DISABLE_DISPATCH
            0x0A, 0x05, 0x01, 0x00, // var5 = 1
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());

        vm.step();
        assert!(!vm.dispatch_enabled());
        vm.step();
        assert_eq!(vm.byte_var(5), 0, "the paused frame runs no entries");
        assert!(vm.dispatch_enabled());
        vm.step();
        assert_eq!(vm.byte_var(5), 1);
    }

    #[test]
    fn sfx_waits_while_the_sound_system_is_busy() {
        let script = vec![
            0x20, 0x00, 0x01, 0x00, // SFX
            0x0A, 0x05, 0x01, 0x00, // var5 = 1
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.set_sound_busy(true);

        let frame = vm.step();
        assert!(frame.sfx.is_empty());
        assert!(!vm.dispatch_enabled());

        vm.set_sound_busy(false);
        vm.step(); // re-enables dispatch
        let frame = vm.step();
        assert_eq!(frame.sfx.len(), 1);
        assert_eq!(vm.byte_var(5), 1);
    }

    #[test]
    fn clr_flags2_clears_the_busy_wait() {
        let script = vec![
            0x25, 0x00, // CLR_FLAGS2
            0x20, 0x00, 0x01, 0x00, // SFX
            0x01, 0x00,
        ];
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());
        vm.set_sound_busy(true);

        let frame = vm.step();
        assert_eq!(frame.sfx.len(), 1);
        assert!(!vm.sound_busy());
    }

    #[test]
    fn phase_two_holds_for_five_extra_frames() {
        let script = [0x03u8, 0x00].repeat(8);
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());

        let mut phases = Vec::new();
        for _ in 0..10 {
            let frame = vm.step();
            phases.push((frame.phase, frame.frame, frame.hold));
        }

        assert_eq!(phases[0], (0, 0, 0));
        assert_eq!(phases[1], (1, 1, 0));
        assert_eq!(phases[2], (2, 2, 0));
        assert_eq!(phases[3], (2, 2, 1));
        assert_eq!(phases[7], (2, 2, 5));
        assert_eq!(phases[8], (3, 3, 0));
    }

    #[test]
    fn black_covers_the_first_three_frames() {
        let script = [0x03u8, 0x00].repeat(16);
        let dor = dor_with(vec![script]);
        let mut vm = run(&dor, DoorParams::default());

        // The phase-2 hold freezes the frame counter, so black also covers the
        // hold; it clears on the first frame after the hold.
        let frames: Vec<bool> = (0..10).map(|_| vm.step().black).collect();
        assert_eq!(&frames[..8], [true; 8]);
        assert!(!frames[8]);
    }

    #[test]
    fn entry_camera_is_masked_to_six_bits() {
        let dor = dor_with(vec![vec![0x01, 0x00]]);
        let vm = run(
            &dor,
            DoorParams {
                entry_camera: 0xC1,
                ..DoorParams::default()
            },
        );
        assert_eq!(vm.byte_var(3), 1);
    }

    #[test]
    fn finish_ends_the_animation_immediately() {
        let dor = dor_with(vec![[0x03u8, 0x00].repeat(8)]);
        let mut vm = run(&dor, DoorParams::default());
        vm.step();
        vm.finish();
        assert!(vm.is_done());
        assert!(vm.step().done);
    }
}
