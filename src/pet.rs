use rand::Rng;
use std::time::{Duration, Instant};

use crate::half_block::HbFrame;

pub enum Anim {
    Sixel {
        sixels: Vec<String>,
        durations_ms: Vec<u32>,
    },
    HalfBlock {
        frames: Vec<HbFrame>,
        durations_ms: Vec<u32>,
    },
}

impl Anim {
    pub fn frame_count(&self) -> usize {
        match self {
            Anim::Sixel { sixels, .. } => sixels.len(),
            Anim::HalfBlock { frames, .. } => frames.len(),
        }
    }
    pub fn duration_ms(&self, frame: usize) -> u32 {
        match self {
            Anim::Sixel { durations_ms, .. } | Anim::HalfBlock { durations_ms, .. } => {
                durations_ms[frame]
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    WalkingLeft,
    WalkingRight,
    WalkingUp,
    WalkingDown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Happy,
    Playful,
    Sleepy,
}

const ANIM_WASH: usize = 0;
const ANIM_WALK_RIGHT: usize = 1;
const ANIM_WALK_LEFT: usize = 2;
const ANIM_WALK_UP: usize = 3;
const ANIM_WALK_DOWN: usize = 4;
const ANIM_SLEEP_CURL: usize = 5;
const ANIM_YAWN: usize = 6;
const ANIM_SLEEP_LOAF: usize = 7;
const ANIM_SLEEP_HEAD: usize = 8;
const ANIM_SLEEP_STRETCH: usize = 9;
const ANIM_WASH_LIE: usize = 10;
const ANIM_SCRATCH: usize = 11;

const IDLE_VARIANTS: &[usize] = &[ANIM_WASH, ANIM_YAWN, ANIM_WASH_LIE, ANIM_SCRATCH];
const SLEEP_VARIANTS: &[usize] = &[
    ANIM_SLEEP_CURL,
    ANIM_SLEEP_LOAF,
    ANIM_SLEEP_HEAD,
    ANIM_SLEEP_STRETCH,
];

const IDLE_TO_SLEEP_TICKS: u32 = 100;

pub struct Pet {
    pub row: u16,
    pub col: u16,
    target_row: u16,
    target_col: u16,
    pause_ticks: u16,
    idle_ticks: u32,
    pub last_drawn: Option<(u16, u16, usize, usize)>,
    pub rows_pub: u16,
    pub cols_pub: u16,
    // screen.scroll_count when render_pet last ran. Comparing tells us
    // how many rows the sprite's pixels physically scrolled upward since
    // last redraw — restoration extends upward to clean those ghost rows.
    pub last_render_scroll: u64,
    // Tracks the screen.alt_screen state we last saw so render_pet can
    // detect the alt → normal transition and force a fresh stamp.
    pub was_alt_screen: bool,
    rows: u16,
    cols: u16,
    pub cell_w: u16,
    pub cell_h: u16,
    pub animations: Vec<Anim>,
    state: State,
    active_idle_anim: usize,
    active_sleep_anim: usize,
    pub current_frame: usize,
    last_frame_change: Instant,
    pub mood: Mood,
    mood_ticks: u16,
    zoom_ticks: u16,
}

impl Pet {
    pub fn resize(&mut self, new_rows: u16, new_cols: u16) {
        self.rows = new_rows;
        self.cols = new_cols;
        self.rows_pub = new_rows;
        self.cols_pub = new_cols;
        let max_row = new_rows.saturating_sub(self.cell_h);
        let max_col = new_cols.saturating_sub(self.cell_w);
        if self.row > max_row {
            self.row = max_row;
        }
        if self.col > max_col {
            self.col = max_col;
        }
        if self.target_row > max_row {
            self.target_row = max_row;
        }
        if self.target_col > max_col {
            self.target_col = max_col;
        }
        // Old position is no longer trustworthy.
        self.last_drawn = None;
    }

    pub fn new(rows: u16, cols: u16, animations: Vec<Anim>, cell_w: u16, cell_h: u16) -> Self {
        assert_eq!(
            animations.len(),
            12,
            "expected 12 animations (see main.rs ordering)"
        );
        let start_col = cols.saturating_sub(cell_w);
        let start_row = rows.saturating_sub(cell_h + 2);
        Self {
            row: start_row,
            col: start_col,
            target_row: start_row,
            target_col: start_col,
            pause_ticks: 0,
            idle_ticks: 0,
            last_drawn: None,
            rows_pub: rows,
            cols_pub: cols,
            last_render_scroll: 0,
            was_alt_screen: false,
            rows,
            cols,
            cell_w,
            cell_h,
            animations,
            state: State::Idle,
            active_idle_anim: ANIM_WASH,
            active_sleep_anim: ANIM_SLEEP_CURL,
            current_frame: 0,
            last_frame_change: Instant::now(),
            mood: Mood::Happy,
            mood_ticks: 0,
            zoom_ticks: 0,
        }
    }

    pub fn pet(&mut self) {
        self.mood = Mood::Happy;
        self.mood_ticks = 220;
    }

    pub fn feed(&mut self) {
        self.mood = Mood::Playful;
        self.mood_ticks = 220;
        self.zoom_ticks = 60;
    }

    pub fn apply_time_bias(&mut self, hour: u8) {
        if self.mood_ticks > 0 {
            return;
        }
        self.mood = if (22..=23).contains(&hour) || (0..=6).contains(&hour) {
            Mood::Sleepy
        } else if (7..=17).contains(&hour) {
            Mood::Playful
        } else {
            Mood::Happy
        };
    }

    pub fn bbox(&self) -> (u16, u16, u16, u16) {
        (self.row, self.col, self.cell_h, self.cell_w)
    }

    pub fn anim_index(&self) -> usize {
        match self.state {
            State::Idle => {
                if self.idle_ticks >= IDLE_TO_SLEEP_TICKS || self.mood == Mood::Sleepy {
                    self.active_sleep_anim
                } else {
                    self.active_idle_anim
                }
            }
            State::WalkingRight => ANIM_WALK_RIGHT,
            State::WalkingLeft => ANIM_WALK_LEFT,
            State::WalkingUp => ANIM_WALK_UP,
            State::WalkingDown => ANIM_WALK_DOWN,
        }
    }

    pub fn current_anim(&self) -> &Anim {
        &self.animations[self.anim_index()]
    }

    fn set_state(&mut self, s: State) {
        if self.state != s {
            self.state = s;
            self.current_frame = 0;
            self.last_frame_change = Instant::now();
            if s != State::Idle {
                self.idle_ticks = 0;
            }
        }
    }

    fn intersects_box(&self, row: u16, col: u16, other: (u16, u16, u16, u16)) -> bool {
        let (or, oc, oh, ow) = other;
        let a_top = row;
        let a_left = col;
        let a_bottom = row.saturating_add(self.cell_h);
        let a_right = col.saturating_add(self.cell_w);
        let b_top = or;
        let b_left = oc;
        let b_bottom = or.saturating_add(oh);
        let b_right = oc.saturating_add(ow);
        !(a_right <= b_left || b_right <= a_left || a_bottom <= b_top || b_bottom <= a_top)
    }

    fn is_blocked(&self, row: u16, col: u16, occupied: &[(u16, u16, u16, u16)]) -> bool {
        occupied
            .iter()
            .any(|&other| self.intersects_box(row, col, other))
    }

    pub fn tick(&mut self, occupied: &[(u16, u16, u16, u16)], event_mode: bool) {
        if self.mood_ticks > 0 {
            self.mood_ticks -= 1;
            if self.mood_ticks == 0 {
                self.mood = Mood::Happy;
            }
        }
        if event_mode && self.zoom_ticks == 0 && rand::thread_rng().gen_bool(0.005) {
            self.zoom_ticks = 50;
            self.mood = Mood::Playful;
        }

        if self.state == State::Idle {
            self.idle_ticks = self.idle_ticks.saturating_add(1);
        }
        let cur_anim = self.anim_index();
        let frame_count = self.animations[cur_anim].frame_count();
        if self.current_frame >= frame_count {
            self.current_frame = 0;
            self.last_frame_change = Instant::now();
        }
        let mut dur_ms = self.animations[cur_anim]
            .duration_ms(self.current_frame)
            .max(50) as u64;
        dur_ms = match self.mood {
            Mood::Playful => dur_ms.saturating_mul(2) / 3,
            Mood::Sleepy => dur_ms.saturating_mul(3) / 2,
            Mood::Happy => dur_ms,
        };
        if self.last_frame_change.elapsed() >= Duration::from_millis(dur_ms.max(40)) {
            self.current_frame = (self.current_frame + 1) % frame_count;
            self.last_frame_change = Instant::now();
        }

        if self.pause_ticks > 0 {
            if self.zoom_ticks > 0 {
                self.zoom_ticks -= 1;
            }
            self.pause_ticks -= 1;
            return;
        }

        if self.row == self.target_row && self.col == self.target_col {
            self.set_state(State::Idle);
            let mut rng = rand::thread_rng();
            self.active_idle_anim = IDLE_VARIANTS[rng.gen_range(0..IDLE_VARIANTS.len())];
            self.active_sleep_anim = SLEEP_VARIANTS[rng.gen_range(0..SLEEP_VARIANTS.len())];
            let max_row = self.rows.saturating_sub(self.cell_h).max(1);
            let max_col = self.cols.saturating_sub(self.cell_w).max(1);
            self.target_row = rng.gen_range(0..max_row);
            self.target_col = rng.gen_range(0..max_col);
            self.pause_ticks = match self.mood {
                Mood::Sleepy => rng.gen_range(220..420),
                Mood::Playful => rng.gen_range(8..30),
                Mood::Happy => {
                    if rng.gen_bool(0.25) {
                        rng.gen_range(250..500)
                    } else {
                        rng.gen_range(20..60)
                    }
                }
            };
            return;
        }

        let steps = if self.zoom_ticks > 0 { 2 } else { 1 };
        if self.zoom_ticks > 0 {
            self.zoom_ticks -= 1;
        }

        for _ in 0..steps {
            if self.col != self.target_col {
                if self.col < self.target_col {
                    let next_col = self.col.saturating_add(1);
                    if self.is_blocked(self.row, next_col, occupied) {
                        self.target_col = self.col;
                        self.pause_ticks = 10;
                        break;
                    }
                    self.set_state(State::WalkingRight);
                    self.col = next_col;
                } else {
                    let next_col = self.col.saturating_sub(1);
                    if self.is_blocked(self.row, next_col, occupied) {
                        self.target_col = self.col;
                        self.pause_ticks = 10;
                        break;
                    }
                    self.set_state(State::WalkingLeft);
                    self.col = next_col;
                }
            } else if self.row != self.target_row {
                if self.row < self.target_row {
                    let next_row = self.row.saturating_add(1);
                    if self.is_blocked(next_row, self.col, occupied) {
                        self.target_row = self.row;
                        self.pause_ticks = 10;
                        break;
                    }
                    self.set_state(State::WalkingDown);
                    self.row = next_row;
                } else {
                    let next_row = self.row.saturating_sub(1);
                    if self.is_blocked(next_row, self.col, occupied) {
                        self.target_row = self.row;
                        self.pause_ticks = 10;
                        break;
                    }
                    self.set_state(State::WalkingUp);
                    self.row = next_row;
                }
            }
            if self.row == self.target_row && self.col == self.target_col {
                break;
            }
        }
    }
}
