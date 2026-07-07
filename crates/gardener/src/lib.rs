//! Pure branch hygiene planning for coven-github.

pub mod report;
pub mod scan;
pub mod schedule;

pub use report::{ExecutionCounts, RunReport};
pub use scan::{
    classify, matches_exclude, plan, Autonomy, BranchClass, BranchFacts, GardenPlan,
    GardenerPolicy, PruneAction, PruneReason, SkipCode, SkipReason, SurfaceAction,
};
pub use schedule::{parse_schedule, Schedule, ScheduleError};
