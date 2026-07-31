//! Pure derivation of a [`TraySummary`] from a [`Sample`] plus the
//! hysteresis state that keeps the CPU-pressure rail from flapping.

use tairix_abi::switchboard_ipc::{
    TrayPermille, TrayPressure, TrayPressureCount, TrayPressureKind, TraySummary, TrayTask,
    TrayTaskName,
};

use crate::sample::Sample;

/// The busy-fraction threshold (in permille) at which CPU pressure is
/// entered.
///
/// Chosen high enough (90%) that ordinary bursts never trip it, so the
/// rail names sustained saturation rather than a momentary spike.
pub const CPU_PRESSURE_ENTER_PERMILLE: u16 = 900;

/// The busy-fraction threshold (in permille) below which CPU pressure is
/// cleared.
///
/// Deliberately below [`CPU_PRESSURE_ENTER_PERMILLE`] (80%, not 90%): the
/// ten-point gap is hysteresis, so a load hovering right at the enter
/// threshold does not flap the tray icon's pressure rail on and off every
/// sample.
pub const CPU_PRESSURE_EXIT_PERMILLE: u16 = 800;

/// The memory-pressure band depth at and above which the resource counts as
/// pressured (`sysinfo`'s `1`, "mild", and deeper).
///
/// Band `0` is "normal"; the kernel's own enter/exit watermarks already
/// give the band itself hysteresis, so no further software hysteresis is
/// layered on top here.
pub const MEMORY_PRESSURE_BAND_THRESHOLD: u8 = 1;

/// Carries the one stateful decision [`derive_summary`] makes across calls:
/// whether CPU pressure is currently latched active, so the enter/exit
/// permille thresholds form a hysteresis band rather than a single flappy
/// crossing point.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Hysteresis {
    cpu_pressure_active: bool,
}

impl Hysteresis {
    /// A fresh hysteresis state: CPU pressure starts inactive.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_pressure_active: false,
        }
    }
}

/// Derive a [`TraySummary`] from `sample`, threading `hysteresis` across
/// calls so the CPU-pressure rail only flips at its enter/exit thresholds.
///
/// * `jobs` is always `0`: no background-job registry exists in the OS
///   today, so the field stays an honest zero rather than a fabricated
///   count.
/// * `recovery` is the sample's stopped-process count.
/// * `cpu_busy_permille` is the sample's measured busy fraction, or the
///   honest zero permille on an unmeasurable interval.
/// * `pressure` is the more severe of the CPU and memory pressure signals
///   (a tie favours CPU, since a saturated CPU is the more directly
///   actionable of the two), naming how many distinct resources are
///   pressured.
/// * `top_task` is the sample's leading task, provided its name validates
///   as a [`TrayTaskName`]; a name that fails validation (empty, over-long,
///   invalid UTF-8, or containing a control character) yields no top task
///   rather than a fabricated one.
#[must_use]
pub fn derive_summary(sample: &Sample, hysteresis: &mut Hysteresis) -> TraySummary {
    let cpu_busy_raw = sample.cpu_busy_permille.unwrap_or(0);

    if hysteresis.cpu_pressure_active {
        if cpu_busy_raw < CPU_PRESSURE_EXIT_PERMILLE {
            hysteresis.cpu_pressure_active = false;
        }
    } else if cpu_busy_raw >= CPU_PRESSURE_ENTER_PERMILLE {
        hysteresis.cpu_pressure_active = true;
    }

    let cpu_pressure = hysteresis
        .cpu_pressure_active
        .then(|| (TrayPressureKind::Cpu, permille(cpu_busy_raw)));

    let memory_pressure = sample.memory_pressure.and_then(|memory| {
        if memory.band < MEMORY_PRESSURE_BAND_THRESHOLD {
            return None;
        }
        Some((TrayPressureKind::Memory, permille(memory.used_permille)))
    });

    let pressure = dominant_pressure(cpu_pressure, memory_pressure);

    let top_task = sample.top_task.as_ref().and_then(|top| {
        let name_str = core::str::from_utf8(&top.name).ok()?;
        let name = TrayTaskName::new(name_str).ok()?;
        Some(TrayTask {
            name,
            cpu_permille: permille(top.cpu_permille),
        })
    });

    TraySummary {
        jobs: 0,
        recovery: sample.stopped_count,
        cpu_busy_permille: permille(cpu_busy_raw),
        pressure,
        top_task,
    }
}

/// Build a [`TrayPermille`] from a raw permille value, clamping to
/// [`TrayPermille::FULL`] on the unreachable-in-practice case of an
/// out-of-range input (every caller here already bounds its input to
/// `0..=1000`) rather than ever panicking on a wire-bound construction.
fn permille(raw: u16) -> TrayPermille {
    TrayPermille::new(raw).unwrap_or(TrayPermille::FULL)
}

/// Pick the dominant pressure and its count from up to two candidate
/// `(kind, level)` pairs (CPU, memory): the higher `level` wins, a tie
/// favours CPU.
fn dominant_pressure(
    cpu: Option<(TrayPressureKind, TrayPermille)>,
    memory: Option<(TrayPressureKind, TrayPermille)>,
) -> Option<TrayPressure> {
    let pressured = u8::from(cpu.is_some()) + u8::from(memory.is_some());
    let count = TrayPressureCount::new(pressured).ok()?;
    let (kind, level) = match (cpu, memory) {
        (Some(cpu), Some(memory)) => {
            if cpu.1.as_u16() >= memory.1.as_u16() {
                cpu
            } else {
                memory
            }
        }
        (Some(cpu), None) => cpu,
        (None, Some(memory)) => memory,
        (None, None) => return None,
    };
    Some(TrayPressure { kind, level, count })
}

#[cfg(test)]
#[path = "derive_tests.rs"]
mod tests;
