use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

const DEFAULT_IDLE_THRESHOLD_MINUTES: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourceMode {
    #[default]
    OnlyWhenIdle,
    AlwaysOnLowPriority,
    ScheduledHours,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePreset {
    Low,
    #[default]
    Balanced,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSchedule {
    /// Local weekdays: Monday=1 through Sunday=7.
    pub weekdays: Vec<u8>,
    /// Local wall-clock time in HH:MM form.
    pub start: String,
    /// Local wall-clock time in HH:MM form.
    pub end: String,
}

impl Default for ResourceSchedule {
    fn default() -> Self {
        Self {
            weekdays: vec![1, 2, 3, 4, 5, 6, 7],
            start: "09:00".to_string(),
            end: "17:00".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSettings {
    pub mode: ResourceMode,
    pub idle_threshold_minutes: u8,
    pub schedule: Option<ResourceSchedule>,
    pub preset: ResourcePreset,
    pub do_not_run_on_battery: bool,
    pub not_recommended_confirmed: bool,
    pub performance_percent: u8,
    pub below_normal_priority: bool,
}

impl Default for ResourceSettings {
    fn default() -> Self {
        Self {
            mode: ResourceMode::OnlyWhenIdle,
            idle_threshold_minutes: DEFAULT_IDLE_THRESHOLD_MINUTES,
            schedule: None,
            preset: ResourcePreset::Balanced,
            do_not_run_on_battery: true,
            not_recommended_confirmed: false,
            performance_percent: 50,
            below_normal_priority: true,
        }
    }
}

impl ResourceSettings {
    pub fn validate(&self) -> Result<()> {
        if !(1..=60).contains(&self.idle_threshold_minutes) {
            bail!("idle_threshold_minutes must be between 1 and 60");
        }
        if !(0..=100).contains(&self.performance_percent) {
            bail!("performance_percent must be between 0 and 100");
        }
        if let Some(schedule) = &self.schedule {
            validate_schedule(schedule)?;
        }
        if self.mode == ResourceMode::ScheduledHours && self.schedule.is_none() {
            bail!("schedule is required when mode is scheduled_hours");
        }
        Ok(())
    }

    pub fn requires_restart(&self, next: &Self) -> bool {
        let current_priority = self.preset.mapping(
            8,
            self.mode == ResourceMode::AlwaysOnLowPriority || self.below_normal_priority,
        );
        let next_priority = next.preset.mapping(
            8,
            next.mode == ResourceMode::AlwaysOnLowPriority || next.below_normal_priority,
        );
        self.preset != next.preset
            || self.below_normal_priority != next.below_normal_priority
            || current_priority.process_priority != next_priority.process_priority
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DutyCyclePause {
    NoWork,
    PauseMs(u64),
}

pub fn duty_cycle_pause_ms(
    job_duration_ms: u128,
    performance_percent: i16,
) -> Result<DutyCyclePause> {
    if !(0..=100).contains(&performance_percent) {
        bail!("performance_percent must be between 0 and 100");
    }
    if performance_percent == 0 {
        return Ok(DutyCyclePause::NoWork);
    }
    let percent = u128::try_from(performance_percent)?;
    let pause_ms = job_duration_ms.saturating_mul(100 - percent) / percent;
    // Cap the cooldown at 30 minutes so an unusually long job cannot strand the provider.
    let pause_ms = pause_ms.min(30 * 60 * 1000);
    Ok(DutyCyclePause::PauseMs(u64::try_from(pause_ms)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PresetMapping {
    pub num_thread: usize,
    pub keep_alive: &'static str,
    pub ollama_num_parallel: u8,
    pub max_loaded_models: u8,
    pub process_priority: ProcessPriority,
    pub priority_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPriority {
    Normal,
    BelowNormal,
}

impl ResourcePreset {
    pub fn mapping(self, logical_cpu_count: usize, below_normal_mode: bool) -> PresetMapping {
        let threads = logical_cpu_count.max(1);
        let (num_thread, keep_alive, preset_priority) = match self {
            Self::Low => (1, "0", ProcessPriority::BelowNormal),
            Self::Balanced => ((threads / 2).max(1), "5m", ProcessPriority::Normal),
            Self::High => (threads, "15m", ProcessPriority::Normal),
        };
        let process_priority = if below_normal_mode {
            ProcessPriority::BelowNormal
        } else {
            preset_priority
        };
        PresetMapping {
            num_thread,
            keep_alive,
            ollama_num_parallel: 1,
            max_loaded_models: 1,
            process_priority,
            priority_supported: cfg!(target_os = "windows"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    Active,
    IdleFor(Duration),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Ac,
    OnBattery,
    Unknown { battery_present: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyInput {
    pub idle: IdleState,
    pub power: PowerState,
    pub schedule_active: bool,
    pub user_paused: bool,
    pub mode: ResourceMode,
    pub idle_threshold: Duration,
    pub do_not_run_on_battery: bool,
    pub performance_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum PolicyDecision {
    Run,
    Paused(String),
}

pub fn decide_policy(input: PolicyInput) -> PolicyDecision {
    if input.user_paused {
        return PolicyDecision::Paused("Paused by user".to_string());
    }
    if input.performance_percent == 0 {
        return PolicyDecision::Paused("Performance is set to 0%, no work is accepted".to_string());
    }
    if input.do_not_run_on_battery {
        match input.power {
            PowerState::OnBattery => {
                return PolicyDecision::Paused(
                    "Paused because the device is on battery power".into(),
                )
            }
            PowerState::Unknown {
                battery_present: true,
            } => {
                return PolicyDecision::Paused(
                    "Paused because battery power status is unknown on a device with a battery"
                        .into(),
                )
            }
            PowerState::Ac
            | PowerState::Unknown {
                battery_present: false,
            } => {}
        }
    }

    match input.mode {
        ResourceMode::OnlyWhenIdle => match input.idle {
            IdleState::IdleFor(duration) if duration >= input.idle_threshold => PolicyDecision::Run,
            IdleState::Active | IdleState::IdleFor(_) => {
                PolicyDecision::Paused("Waiting for the idle threshold".to_string())
            }
            IdleState::Unknown => PolicyDecision::Paused(
                "Idle status is unavailable; switch to Always on (low priority) to run".into(),
            ),
        },
        ResourceMode::AlwaysOnLowPriority => PolicyDecision::Run,
        ResourceMode::ScheduledHours => {
            if input.schedule_active {
                PolicyDecision::Run
            } else {
                PolicyDecision::Paused("Outside scheduled hours".to_string())
            }
        }
    }
}

pub fn decide_current_policy(
    settings: &ResourceSettings,
    idle: IdleState,
    power: PowerState,
    user_paused: bool,
) -> PolicyDecision {
    let schedule_active = settings
        .schedule
        .as_ref()
        .is_some_and(|schedule| is_within_schedule(schedule, Local::now()));
    decide_policy(PolicyInput {
        idle,
        power,
        schedule_active,
        user_paused,
        mode: settings.mode,
        idle_threshold: Duration::from_secs(u64::from(settings.idle_threshold_minutes) * 60),
        do_not_run_on_battery: settings.do_not_run_on_battery,
        performance_percent: settings.performance_percent,
    })
}

pub fn validate_schedule(schedule: &ResourceSchedule) -> Result<()> {
    if schedule.weekdays.is_empty()
        || schedule
            .weekdays
            .iter()
            .any(|weekday| !(1..=7).contains(weekday))
    {
        bail!("schedule weekdays must contain values from 1 (Monday) to 7 (Sunday)");
    }
    if schedule
        .weekdays
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != schedule.weekdays.len()
    {
        bail!("schedule weekdays must not contain duplicates");
    }
    let start = parse_local_time(&schedule.start)?;
    let end = parse_local_time(&schedule.end)?;
    if start == end {
        bail!("schedule start and end times must be different");
    }
    Ok(())
}

fn parse_local_time(value: &str) -> Result<u16> {
    let Some((hours, minutes)) = value.split_once(':') else {
        bail!("schedule time must use HH:MM format");
    };
    if hours.len() != 2 || minutes.len() != 2 {
        bail!("schedule time must use HH:MM format");
    }
    let hours = hours
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("schedule time contains invalid digits"))?;
    let minutes = minutes
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("schedule time contains invalid digits"))?;
    if hours > 23 || minutes > 59 {
        bail!("schedule time must be a valid 24-hour time");
    }
    Ok(hours * 60 + minutes)
}

pub fn is_within_schedule<Tz: TimeZone>(schedule: &ResourceSchedule, now: DateTime<Tz>) -> bool {
    let minute = (now.hour() * 60 + now.minute()) as u16;
    let weekday = now.weekday().number_from_monday() as u8;
    let start = match parse_local_time(&schedule.start) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let end = match parse_local_time(&schedule.end) {
        Ok(value) => value,
        Err(_) => return false,
    };
    if start < end {
        schedule.weekdays.contains(&weekday) && (start..end).contains(&minute)
    } else if minute >= start {
        schedule.weekdays.contains(&weekday)
    } else {
        let previous_weekday = if weekday == 1 { 7 } else { weekday - 1 };
        minute < end && schedule.weekdays.contains(&previous_weekday)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use chrono_tz::America::New_York;

    use super::{
        decide_policy, duty_cycle_pause_ms, is_within_schedule, DutyCyclePause, IdleState,
        PolicyDecision, PolicyInput, PowerState, ResourceMode, ResourcePreset, ResourceSchedule,
        ResourceSettings,
    };

    fn input(mode: ResourceMode) -> PolicyInput {
        PolicyInput {
            idle: IdleState::Active,
            power: PowerState::Ac,
            schedule_active: false,
            user_paused: false,
            mode,
            idle_threshold: Duration::from_secs(60),
            do_not_run_on_battery: true,
            performance_percent: 50,
        }
    }

    #[test]
    fn defaults_use_idle_mode_five_minutes_and_battery_protection() {
        let settings = ResourceSettings::default();
        assert_eq!(settings.mode, ResourceMode::OnlyWhenIdle);
        assert_eq!(settings.idle_threshold_minutes, 5);
        assert_eq!(settings.preset, ResourcePreset::Balanced);
        assert!(settings.do_not_run_on_battery);
        assert!(!settings.not_recommended_confirmed);
        assert_eq!(settings.performance_percent, 50);
        assert!(settings.below_normal_priority);
        assert!(settings.schedule.is_none());
    }

    #[test]
    fn validates_performance_slider_bounds() {
        for value in [0, 1, 50, 100] {
            assert!(ResourceSettings {
                performance_percent: value,
                ..ResourceSettings::default()
            }
            .validate()
            .is_ok());
        }
        for value in [101, u8::MAX] {
            assert!(ResourceSettings {
                performance_percent: value,
                ..ResourceSettings::default()
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn duty_cycle_scales_the_post_job_pause() {
        assert_eq!(
            duty_cycle_pause_ms(10_000, 0).unwrap(),
            DutyCyclePause::NoWork
        );
        assert_eq!(
            duty_cycle_pause_ms(10_000, 1).unwrap(),
            DutyCyclePause::PauseMs(990_000)
        );
        assert_eq!(
            duty_cycle_pause_ms(10_000, 50).unwrap(),
            DutyCyclePause::PauseMs(10_000)
        );
        assert_eq!(
            duty_cycle_pause_ms(10_000, 99).unwrap(),
            DutyCyclePause::PauseMs(101)
        );
        assert_eq!(
            duty_cycle_pause_ms(10_000, 100).unwrap(),
            DutyCyclePause::PauseMs(0)
        );
        assert_eq!(
            duty_cycle_pause_ms(60_000, 1).unwrap(),
            DutyCyclePause::PauseMs(30 * 60 * 1000)
        );
        assert!(duty_cycle_pause_ms(10_000, -1).is_err());
        assert!(duty_cycle_pause_ms(10_000, 101).is_err());
        assert!(duty_cycle_pause_ms(10_000, 255).is_err());
    }

    #[test]
    fn validates_settings_and_schedule_ranges() {
        assert!(ResourceSettings {
            idle_threshold_minutes: 0,
            ..ResourceSettings::default()
        }
        .validate()
        .is_err());
        assert!(ResourceSettings {
            idle_threshold_minutes: 61,
            ..ResourceSettings::default()
        }
        .validate()
        .is_err());
        let scheduled = |schedule| ResourceSettings {
            mode: ResourceMode::ScheduledHours,
            schedule,
            ..ResourceSettings::default()
        };
        assert!(scheduled(None).validate().is_err());
        assert!(scheduled(Some(ResourceSchedule {
            weekdays: vec![1, 1],
            start: "09:00".into(),
            end: "17:00".into(),
        }))
        .validate()
        .is_err());
        assert!(scheduled(Some(ResourceSchedule {
            weekdays: vec![1],
            start: "09:00".into(),
            end: "09:00".into(),
        }))
        .validate()
        .is_err());
        assert!(scheduled(Some(ResourceSchedule {
            weekdays: vec![1, 7],
            start: "22:30".into(),
            end: "06:00".into(),
        }))
        .validate()
        .is_ok());
    }

    #[test]
    fn pause_and_battery_rules_take_precedence() {
        let mut paused = input(ResourceMode::AlwaysOnLowPriority);
        paused.user_paused = true;
        paused.performance_percent = 0;
        assert_eq!(
            decide_policy(paused),
            PolicyDecision::Paused("Paused by user".to_string())
        );

        let mut no_work = input(ResourceMode::AlwaysOnLowPriority);
        no_work.performance_percent = 0;
        assert_eq!(
            decide_policy(no_work),
            PolicyDecision::Paused("Performance is set to 0%, no work is accepted".to_string())
        );

        let mut on_battery = input(ResourceMode::AlwaysOnLowPriority);
        on_battery.power = PowerState::OnBattery;
        assert!(matches!(
            decide_policy(on_battery),
            PolicyDecision::Paused(_)
        ));

        let mut unknown_battery = input(ResourceMode::AlwaysOnLowPriority);
        unknown_battery.power = PowerState::Unknown {
            battery_present: true,
        };
        assert!(matches!(
            decide_policy(unknown_battery),
            PolicyDecision::Paused(_)
        ));

        let mut unknown_desktop = input(ResourceMode::AlwaysOnLowPriority);
        unknown_desktop.power = PowerState::Unknown {
            battery_present: false,
        };
        assert_eq!(decide_policy(unknown_desktop), PolicyDecision::Run);
    }

    #[test]
    fn idle_threshold_and_unknown_idle_follow_mode_rules() {
        let mut policy = input(ResourceMode::OnlyWhenIdle);
        policy.idle = IdleState::IdleFor(Duration::from_secs(59));
        assert!(matches!(decide_policy(policy), PolicyDecision::Paused(_)));
        policy.idle = IdleState::IdleFor(Duration::from_secs(60));
        assert_eq!(decide_policy(policy), PolicyDecision::Run);
        policy.idle = IdleState::Unknown;
        assert!(matches!(decide_policy(policy), PolicyDecision::Paused(_)));
        policy.mode = ResourceMode::AlwaysOnLowPriority;
        assert_eq!(decide_policy(policy), PolicyDecision::Run);
    }

    #[test]
    fn scheduled_mode_requires_an_active_local_window() {
        let mut policy = input(ResourceMode::ScheduledHours);
        assert!(matches!(decide_policy(policy), PolicyDecision::Paused(_)));
        policy.schedule_active = true;
        assert_eq!(decide_policy(policy), PolicyDecision::Run);
    }

    #[test]
    fn schedule_uses_local_weekdays_and_supports_overnight_windows() {
        let schedule = ResourceSchedule {
            weekdays: vec![7],
            start: "22:00".into(),
            end: "02:00".into(),
        };
        let sunday_late = New_York.with_ymd_and_hms(2026, 11, 22, 23, 0, 0).unwrap();
        let monday_early = New_York.with_ymd_and_hms(2026, 11, 23, 1, 0, 0).unwrap();
        let monday_late = New_York.with_ymd_and_hms(2026, 11, 23, 23, 0, 0).unwrap();
        assert!(is_within_schedule(&schedule, sunday_late));
        assert!(is_within_schedule(&schedule, monday_early));
        assert!(!is_within_schedule(&schedule, monday_late));
    }

    #[test]
    fn schedule_is_evaluated_in_local_wall_time_across_dst_changes() {
        let fall_back = ResourceSchedule {
            weekdays: vec![7],
            start: "01:00".into(),
            end: "02:00".into(),
        };
        let ambiguous_one_am = New_York.with_ymd_and_hms(2026, 11, 1, 1, 30, 0);
        let first_one_am = ambiguous_one_am.earliest().unwrap();
        let repeated_one_am = ambiguous_one_am.latest().unwrap();
        assert!(is_within_schedule(&fall_back, first_one_am));
        assert!(is_within_schedule(&fall_back, repeated_one_am));

        let spring_forward = ResourceSchedule {
            weekdays: vec![7],
            start: "02:00".into(),
            end: "03:30".into(),
        };
        let after_gap = New_York
            .with_ymd_and_hms(2026, 3, 8, 3, 15, 0)
            .single()
            .unwrap();
        assert!(is_within_schedule(&spring_forward, after_gap));

        let utc = Utc.with_ymd_and_hms(2026, 3, 8, 7, 15, 0).unwrap();
        assert_eq!(
            after_gap.with_timezone(&Utc),
            utc,
            "test instant should represent the post-DST local time"
        );
    }

    #[test]
    fn presets_have_explicit_mapping_and_require_restart_when_changed() {
        let low = ResourcePreset::Low.mapping(8, false);
        assert_eq!(low.num_thread, 1);
        assert_eq!(low.keep_alive, "0");
        assert_eq!(low.ollama_num_parallel, 1);
        assert_eq!(low.max_loaded_models, 1);
        assert_eq!(low.process_priority, super::ProcessPriority::BelowNormal);

        let balanced = ResourcePreset::Balanced.mapping(8, false);
        assert_eq!(balanced.num_thread, 4);
        assert_eq!(balanced.keep_alive, "5m");
        assert!(
            ResourceSettings::default().requires_restart(&ResourceSettings {
                preset: ResourcePreset::High,
                ..ResourceSettings::default()
            })
        );
    }
}
