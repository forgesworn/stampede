//! Training lessons on the Training Ground (spec 9).

use serde::{Deserialize, Serialize};

/// The six lessons, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LessonId {
    /// 1. Pass through 8 gates.
    RunSteer,
    /// 2. Release three drifts held for at least 0.8 seconds.
    DriftBoost,
    /// 3. Clear 5 hurdles without stumbling.
    Jump,
    /// 4. Destroy 8 Power Eggs in 40 s.
    LaserEyes,
    /// 5. Bash a dummy ostrich 3 times.
    HeadBash,
    /// 6. Collect 10 coins and use 2 items.
    CoinsItems,
}

/// The on-screen control a lesson's hint draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HintControl {
    /// Steer pad.
    Steer,
    /// Drift button.
    Drift,
    /// Jump button.
    Jump,
    /// Laser button.
    Laser,
    /// Bash button / flick.
    Bash,
    /// Item button.
    Item,
}

impl LessonId {
    /// All lessons in order.
    pub const ALL: [LessonId; 6] = [
        LessonId::RunSteer,
        LessonId::DriftBoost,
        LessonId::Jump,
        LessonId::LaserEyes,
        LessonId::HeadBash,
        LessonId::CoinsItems,
    ];

    /// 1-based lesson number.
    pub fn number(self) -> u8 {
        LessonId::ALL.iter().position(|&l| l == self).unwrap_or(0) as u8 + 1
    }

    /// Title.
    pub fn title(self) -> &'static str {
        match self {
            LessonId::RunSteer => "Run & Steer",
            LessonId::DriftBoost => "Drift & Boost",
            LessonId::Jump => "Jump",
            LessonId::LaserEyes => "Laser Eyes",
            LessonId::HeadBash => "Head-Bash",
            LessonId::CoinsItems => "Coins & Items",
        }
    }

    /// One-line instruction.
    pub fn instruction(self) -> &'static str {
        match self {
            LessonId::RunSteer => "Run through all 8 gates.",
            LessonId::DriftBoost => "Hold Drift for a second, then let go for a boost. Do it 3 times.",
            LessonId::Jump => "Jump over 5 log hurdles without tripping.",
            LessonId::LaserEyes => "Set 8 opponents’ tails alight in 40 seconds.",
            LessonId::HeadBash => "Pull alongside a dummy ostrich and bash it 3 times.",
            LessonId::CoinsItems => "Collect 10 bitcoins and use 2 items.",
        }
    }

    /// Short hint shown next to the drawn control.
    pub fn hint(self) -> &'static str {
        match self {
            LessonId::RunSteer => "Drag the left pad to steer",
            LessonId::DriftBoost => "Steer + hold Drift for a second, then let go",
            LessonId::Jump => "Tap Jump just before a log",
            LessonId::LaserEyes => "Aim straight at a rival and hold Laser. Watch the heat ring",
            LessonId::HeadBash => "Tap Bash or flick toward them",
            LessonId::CoinsItems => "Run into eggs for items, tap Item to use",
        }
    }

    /// The control the hint draws.
    pub fn control(self) -> HintControl {
        match self {
            LessonId::RunSteer => HintControl::Steer,
            LessonId::DriftBoost => HintControl::Drift,
            LessonId::Jump => HintControl::Jump,
            LessonId::LaserEyes => HintControl::Laser,
            LessonId::HeadBash => HintControl::Bash,
            LessonId::CoinsItems => HintControl::Item,
        }
    }

    /// Goal count.
    pub fn goal(self) -> u32 {
        match self {
            LessonId::RunSteer => 8,
            LessonId::DriftBoost => 3,
            LessonId::Jump => 5,
            LessonId::LaserEyes => 8,
            LessonId::HeadBash => 3,
            LessonId::CoinsItems => 12,
        }
    }

    /// Time limit, if any.
    pub fn time_limit(self) -> Option<f32> {
        match self {
            LessonId::LaserEyes => Some(40.0),
            _ => None,
        }
    }

    /// Finish within `[0]` s for 3 stars, `[1]` s for 2 stars, else 1 star.
    pub fn star_times(self) -> [f32; 2] {
        match self {
            LessonId::RunSteer => [22.0, 32.0],
            LessonId::DriftBoost => [30.0, 50.0],
            LessonId::Jump => [35.0, 55.0],
            LessonId::LaserEyes => [25.0, 33.0],
            LessonId::HeadBash => [25.0, 45.0],
            LessonId::CoinsItems => [45.0, 75.0],
        }
    }
}

/// A lesson in progress (`World::lesson`). The sim updates it from its own events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LessonState {
    /// Which lesson.
    pub id: LessonId,
    /// Goal counter.
    pub progress: u32,
    /// Goal.
    pub goal: u32,
    /// Completed.
    pub done: bool,
    /// Ran out of time.
    pub failed: bool,
    /// 1-3 once done.
    pub stars: u8,
    /// Time since GO.
    pub time: f32,
    /// Lesson 6: coins collected so far.
    pub coins: u32,
    /// Lesson 6: items used so far.
    pub items: u32,
}

impl LessonState {
    /// A fresh lesson.
    pub fn new(id: LessonId) -> Self {
        LessonState {
            id,
            progress: 0,
            goal: id.goal(),
            done: false,
            failed: false,
            stars: 0,
            time: 0.0,
            coins: 0,
            items: 0,
        }
    }

    /// Stars for finishing at `time`.
    pub fn stars_for(id: LessonId, time: f32) -> u8 {
        let [a, b] = id.star_times();
        if time <= a {
            3
        } else if time <= b {
            2
        } else {
            1
        }
    }
}
