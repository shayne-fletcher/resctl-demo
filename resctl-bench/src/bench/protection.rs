// Copyright (c) Facebook, Inc. and its affiliates.
use super::*;
use rd_agent_intf::{bandit_report::BanditMemHogReport, Report};
use std::collections::{BTreeMap, VecDeque};
use std::cell::RefCell;

#[derive(Clone, Copy, Debug)]
pub enum MemHogSpeed {
    Hog10Pct,
    Hog25Pct,
    Hog50Pct,
    Hog1x,
    Hog2x,
}

impl MemHogSpeed {
    fn from_str(input: &str) -> Result<Self> {
        Ok(match input {
            "10%" => Self::Hog10Pct,
            "25%" => Self::Hog25Pct,
            "50%" => Self::Hog50Pct,
            "1x" => Self::Hog1x,
            "2x" => Self::Hog2x,
            _ => bail!("\"speed\" should be one of 10%, 25%, 50%, 1x or 2x"),
        })
    }

    fn to_sideload_name(&self) -> &'static str {
        match self {
            Self::Hog10Pct => "mem-hog-10pct",
            Self::Hog25Pct => "mem-hog-25pct",
            Self::Hog50Pct => "mem-hog-50pct",
            Self::Hog1x => "mem-hog-1x",
            Self::Hog2x => "mem-hog-2x",
        }
    }
}

impl std::fmt::Display for MemHogSpeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Hog10Pct => "10%",
                Self::Hog25Pct => "25%",
                Self::Hog50Pct => "50%",
                Self::Hog1x => "1x",
                Self::Hog2x => "2x",
            }
        )
    }
}

fn warm_up_hashd(rctx: &mut RunCtx, load: f64) -> Result<()> {
    rctx.start_hashd(load);
    rctx.stabilize_hashd(Some(load))
}

fn ws_status(mon: &WorkloadMon, af: &AgentFiles) -> Result<(bool, String)> {
    let mut status = String::new();
    let rep = &af.report.data;
    let swap_total = rep.usages[ROOT_SLICE].swap_bytes + rep.usages[ROOT_SLICE].swap_free;
    let swap_usage = match swap_total {
        0 => 0.0,
        total => rep.usages[ROOT_SLICE].swap_bytes as f64 / total as f64,
    };
    write!(
        status,
        "load:{:>4}% lat:{:>5} swap:{:>4}%",
        format_pct(mon.hashd_loads[0]),
        format_duration(rep.hashd[0].lat.ctl),
        format_pct_dashed(swap_usage)
    )
    .unwrap();

    let work = &rep.usages[&Slice::Work.name().to_owned()];
    let sys = &rep.usages[&Slice::Sys.name().to_owned()];
    write!(
        status,
        " w/s mem:{:>5}/{:>5} swap:{:>5}/{:>5} memp:{:>4}%/{:>4}%",
        format_size(work.mem_bytes),
        format_size(sys.mem_bytes),
        format_size(work.swap_bytes),
        format_size(sys.swap_bytes),
        format_pct(work.mem_pressures.1),
        format_pct(sys.mem_pressures.1)
    )
    .unwrap();
    Ok((false, status))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemHogRun {
    pub stable_at: u64,
    pub hog_started_at: u64,
    pub hog_ended_at: u64,
    pub first_hog_rep: BanditMemHogReport,
    pub last_hog_rep: BanditMemHogReport,
    pub last_hog_mem: usize,
}

#[derive(Clone, Debug)]
pub struct MemHog {
    pub loops: u32,
    pub load: f64,
    pub speed: MemHogSpeed,
    pub main_period: (u64, u64),
    pub runs: Vec<MemHogRun>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemHogResult {
    pub base_rps: f64,
    pub base_rps_stdev: f64,
    pub base_lat: f64,
    pub base_lat_stdev: f64,

    pub work_isol_pcts: BTreeMap<String, f64>,
    pub work_isol: f64,
    pub work_isol_stdev: f64,

    pub lat_impact_pcts: BTreeMap<String, f64>,
    pub lat_impact: f64,
    pub lat_impact_stdev: f64,

    pub work_csv: f64,

    pub iolat_pcts: [BTreeMap<String, BTreeMap<String, f64>>; 2],

    pub main_period: (u64, u64),
    pub hog_periods: Vec<(u64, u64)>,
    pub vrate: f64,
    pub vrate_stdev: f64,
    pub io_usage: f64,
    pub io_unused: f64,
    pub hog_io_usage: f64,
    pub hog_io_loss: f64,
    pub hog_bytes: u64,
    pub hog_lost_bytes: u64,

    pub runs: Vec<MemHogRun>,
}

impl MemHog {
    const NAME: &'static str = "mem-hog";
    const DFL_LOOPS: u32 = 3;
    const DFL_LOAD: f64 = 1.0;
    const STABLE_HOLD: f64 = 15.0;
    const TIMEOUT: f64 = 600.0;
    const MEM_AVG_PERIOD: usize = 5;
    const PCTS: [&'static str; 13] = [
        "00", "01", "05", "10", "16", "25", "50", "75", "84", "90", "95", "99", "100",
    ];

    fn read_hog_rep(rep: &Report) -> Result<BanditMemHogReport> {
        let mh_rep_path = match rep.sysloads.get(Self::NAME) {
            Some(sl) => format!("{}/report.json", &sl.scr_path),
            None => bail!("agent report doesn't contain \"mem-hog\" sysload"),
        };
        BanditMemHogReport::load(&mh_rep_path)
            .with_context(|| format!("failed to read bandit-mem-hog report {:?}", &mh_rep_path))
    }

    fn run(&mut self, rctx: &mut RunCtx) -> Result<MemHogResult> {
        self.main_period.0 = unix_now();
        for run_idx in 0..self.loops {
            info!(
                "protection: Stabilizing hashd at {}% for run {}/{}",
                format_pct(self.load),
                run_idx + 1,
                self.loops
            );
            warm_up_hashd(rctx, self.load).context("warming up hashd")?;
            let stable_at = unix_now();

            // hashd stabilized at the target load level. Hold for a bit to
            // guarantee some idle time between runs. These periods are also
            // used to determine the baseline load and latency.
            info!(
                "protection: Holding hashd at {}% for {}",
                format_pct(self.load),
                format_duration(Self::STABLE_HOLD)
            );
            WorkloadMon::default()
                .hashd()
                .timeout(Duration::from_secs_f64(Self::STABLE_HOLD))
                .monitor(rctx)
                .context("holding")?;

            info!("protection: Starting memory hog");
            let hog_started_at = unix_now();
            let timeout = match rctx.test {
                false => Self::TIMEOUT,
                true => 10.0,
            };

            rctx.start_sysload("mem-hog", self.speed.to_sideload_name())?;

            let mh_svc_name = rd_agent_intf::sysload_svc_name(Self::NAME);
            let mut first_hog_rep = Err(anyhow!("swap usage stayed zero"));
            let mut mh_mem_rec = VecDeque::<usize>::new();

            // Memory hog is running. Monitor it until it dies or the
            // timeout expires. Record the memory hog report when the swap
            // usage is first seen, which will be used as the baseline for
            // calculating the total amount of needed and performed IOs.
            WorkloadMon::default()
                .hashd()
                .sysload("mem-hog")
                .timeout(Duration::from_secs_f64(timeout))
                .monitor_with_status(
                    rctx,
                    |wm: &WorkloadMon, af: &AgentFiles| -> Result<(bool, String)> {
                        let rep = &af.report.data;
                        if let (Some(usage), Ok(mh_rep)) =
                            (rep.usages.get(&mh_svc_name), Self::read_hog_rep(rep))
                        {
                            if first_hog_rep.is_err() {
                                if usage.swap_bytes > 0 || rctx.test {
                                    first_hog_rep = Ok(mh_rep);
                                }
                            } else {
                                mh_mem_rec.push_front(usage.mem_bytes as usize);
                                mh_mem_rec.truncate(Self::MEM_AVG_PERIOD);
                            }
                        }
                        ws_status(wm, af)
                    },
                )
                .context("monitoring mem-hog")?;

            // Memory hog is dead. Unwrap the first report and read the last
            // report to calculate delta.
            let first_hog_rep = first_hog_rep?;
            let last_hog_rep =
                rctx.access_agent_files::<_, Result<_>>(|af| Self::read_hog_rep(&af.report.data))?;
            let last_hog_mem = mh_mem_rec.iter().sum::<usize>() / mh_mem_rec.len();

            rctx.stop_sysload("mem-hog");

            let hog_ended_at = unix_now();

            info!("protection: Memory hog terminated");

            self.runs.push(MemHogRun {
                stable_at,
                hog_started_at,
                hog_ended_at,
                first_hog_rep,
                last_hog_rep,
                last_hog_mem,
            });
        }
        self.main_period.1 = unix_now();

        let mut result = Self::study(rctx, self.runs.iter(), self.main_period);
        result.runs = self.runs.clone();
        Ok(result)
    }

    fn calc_work_isol(rps: f64, base_rps: f64) -> f64 {
        (rps / base_rps).min(1.0)
    }

    fn calc_lat_impact(lat: f64, base_lat: f64) -> f64 {
        (lat / base_lat - 1.0).max(0.0)
    }

    fn study<'a, I>(rctx: &RunCtx, runs: I, main_period: (u64, u64)) -> MemHogResult
    where
        I: Iterator<Item = &'a MemHogRun>,
    {
        let runs: Vec<&MemHogRun> = runs.collect();

        // Determine the baseline rps and latency by averaging them over all
        // hold periods. We need these values for the isolation and latency
        // impact studies. Run these first.
        let mut study_base_rps = StudyMean::new(|rep| Some(rep.hashd[0].rps));
        let mut study_base_lat = StudyMean::new(|rep| Some(rep.hashd[0].lat.ctl));

        let mut studies = Studies::new()
            .add(&mut study_base_rps)
            .add(&mut study_base_lat);

        for run in runs.iter() {
            studies.run(rctx, (run.stable_at, run.hog_started_at));
        }

        let (base_rps, base_rps_stdev, _, _) = study_base_rps.result();
        let (base_lat, base_lat_stdev, _, _) = study_base_lat.result();

        // Study work isolation and latency impact. The former is defined as
        // observed rps divided by the baseline, [0.0, 1.0] with 1.0
        // indicating the perfect isolation. The latter is defined as the
        // proportion of the latency increase over the baseline, [0.0, 1.0]
        // with 0.0 indicating no latency impact.
        let mut study_work_isol = StudyMeanPcts::new(
            |rep| Some(Self::calc_work_isol(rep.hashd[0].rps, base_rps)),
            None,
        );
        let mut study_lat_impact = StudyMeanPcts::new(
            |rep| {
                Some(Self::calc_lat_impact(
                    rep.hashd[0].lat.ctl.max(base_lat),
                    base_lat,
                ))
            },
            None,
        );

        // Collect IO usage and unused budgets which will be used to
        // calculate work conservation factor.
        let mh_svc_name = rd_agent_intf::sysload_svc_name(Self::NAME);
        let mut io_usage = 0.0_f64;
        let mut io_unused = 0.0_f64;
        let mut hog_io_usage = 0.0_f64;
        let mut study_io_usages = StudyMutFn::new(|rep| {
            let vrate = rep.iocost.vrate;
            let root_util = rep.usages[ROOT_SLICE].io_util;
            // The reported IO utilization is relative to the effective
            // vrate. Scale so that the values are relative to the
            // absolute model parameters. As vrate is sampled, if it
            // fluctuates at high frequency, this can introduce
            // significant errors.
            io_usage += root_util * vrate / 100.0;
            io_unused += (1.0 - root_util).max(0.0) * vrate / 100.0;
            if let Some(usage) = rep.usages.get(&mh_svc_name) {
                hog_io_usage += usage.io_util * vrate / 100.0;
            }
        });

        // vrate mean isn't used in the process but report to help
        // visibility.
        let mut study_vrate_mean = StudyMean::new(|rep| Some(rep.iocost.vrate));

        let mut study_read_lat_pcts = StudyIoLatPcts::new("read", None);
        let mut study_write_lat_pcts = StudyIoLatPcts::new("write", None);

        let mut studies = Studies::new()
            .add(&mut study_work_isol)
            .add(&mut study_lat_impact)
            .add(&mut study_io_usages)
            .add(&mut study_vrate_mean)
            .add_multiple(&mut study_read_lat_pcts.studies())
            .add_multiple(&mut study_write_lat_pcts.studies());

        let hog_periods: Vec<(u64, u64)> = runs
            .iter()
            .map(|run| {
                (
                    run.first_hog_rep.timestamp.timestamp() as u64,
                    run.last_hog_rep.timestamp.timestamp() as u64,
                )
            })
            .collect();

        for per in hog_periods.iter() {
            studies.run(rctx, *per);
        }

        let (work_isol, work_isol_stdev, work_isol_pcts) = study_work_isol.result(&Self::PCTS);
        let (lat_impact, lat_impact_stdev, lat_impact_pcts) = study_lat_impact.result(&Self::PCTS);

        // Collect how many bytes the memory hogs put out to swap and how
        // much their growth was limited.
        let mut hog_bytes = 0;
        let mut hog_lost_bytes = 0;
        for run in runs.iter() {
            // Total bytes put out to swap is total size sans what was on
            // physical memory.
            hog_bytes += run
                .last_hog_rep
                .wbytes
                .saturating_sub(run.last_hog_mem as u64);
            hog_lost_bytes += run.last_hog_rep.wloss;
        }

        // Determine iocost per each byte and map the number of lost bytes
        // to iocost.
        let hog_cost_per_byte = hog_io_usage as f64 / hog_bytes as f64;
        let hog_io_loss = hog_lost_bytes as f64 * hog_cost_per_byte;

        // If work conservation is 100%, mem-hog would have used all the
        // left over IOs that it could. The conservation factor is defined
        // as the actual usage divided by this maximum possible usage.
        let work_csv = if io_usage > 0.0 {
            let usage_possible = io_usage + io_unused.min(hog_io_loss);
            io_usage as f64 / usage_possible as f64
        } else {
            0.0
        };

        let (vrate, vrate_stdev, _, _) = study_vrate_mean.result();

        let iolat_pcts = [
            study_read_lat_pcts.result(rctx, None),
            study_write_lat_pcts.result(rctx, None),
        ];

        MemHogResult {
            base_rps,
            base_rps_stdev,
            base_lat,
            base_lat_stdev,

            work_isol_pcts,
            work_isol,
            work_isol_stdev,

            lat_impact_pcts,
            lat_impact,
            lat_impact_stdev,

            work_csv,

            iolat_pcts,

            main_period,
            hog_periods,
            vrate,
            vrate_stdev,
            io_usage,
            io_unused,
            hog_io_usage,
            hog_io_loss,
            hog_bytes,
            hog_lost_bytes,

            runs: vec![],
        }
    }

    fn combine_results(rctx: &RunCtx, results: &[&MemHogResult]) -> MemHogResult {
        assert!(results.len() > 0);

        let mut cmb = MemHogResult::default();

        // Combine means, stdevs and sums.
        //
        // means: Average weighted by the number of runs.
        // stdevs: Sqrt of pooled variance by the number of runs.
        // sums: Simple sum.
        let mut total_runs = 0;
        for r in results.iter() {
            total_runs += r.runs.len();

            // Weighted sum for weighted avg calculation.
            let wsum = |c: &mut f64, v: f64| *c += v * (r.runs.len() as f64);
            wsum(&mut cmb.base_rps, r.base_rps);
            wsum(&mut cmb.base_lat, r.base_lat);
            wsum(&mut cmb.work_isol, r.work_isol);
            wsum(&mut cmb.lat_impact, r.lat_impact);
            wsum(&mut cmb.work_csv, r.work_csv);
            wsum(&mut cmb.vrate, r.vrate);

            if r.runs.len() > 1 {
                // Weighted variance sum for pooled variance calculation.
                let vsum = |c: &mut f64, v: f64| *c += v.powi(2) * (r.runs.len() - 1) as f64;
                vsum(&mut cmb.base_rps_stdev, r.base_rps_stdev);
                vsum(&mut cmb.base_lat_stdev, r.base_lat_stdev);
                vsum(&mut cmb.work_isol_stdev, r.work_isol_stdev);
                vsum(&mut cmb.lat_impact_stdev, r.lat_impact_stdev);
                vsum(&mut cmb.vrate_stdev, r.vrate_stdev);
            }

            cmb.main_period = (
                cmb.main_period.0.min(r.main_period.0),
                cmb.main_period.1.max(r.main_period.1),
            );

            cmb.io_usage += r.io_usage;
            cmb.io_unused += r.io_unused;
            cmb.hog_io_usage += r.hog_io_usage;
            cmb.hog_io_loss += r.hog_io_loss;
            cmb.hog_bytes += r.hog_bytes;
            cmb.hog_lost_bytes += r.hog_lost_bytes;

            cmb.hog_periods.append(&mut r.hog_periods.clone());
        }

        let base = total_runs as f64;
        cmb.base_rps /= base;
        cmb.base_lat /= base;
        cmb.work_isol /= base;
        cmb.lat_impact /= base;
        cmb.work_csv /= base;
        cmb.vrate /= base;

        if total_runs > results.len() {
            let base = (total_runs - results.len()) as f64;
            let vsum_to_stdev = |v: &mut f64| *v = (*v / base).sqrt();
            vsum_to_stdev(&mut cmb.base_rps_stdev);
            vsum_to_stdev(&mut cmb.base_lat_stdev);
            vsum_to_stdev(&mut cmb.work_isol_stdev);
            vsum_to_stdev(&mut cmb.lat_impact_stdev);
            vsum_to_stdev(&mut cmb.vrate_stdev);
        }

        // Percentiles can't be combined. Extract them again from the
        // reports. This means that the weighting is different between the
        // combined means and percentiles - the former by the number of
        // runs, the latter the number of data points. While subtle, I think
        // this lends the most useful combined results.
        let base_rps = RefCell::new(0.0_f64);
        let base_lat = RefCell::new(0.0_f64);

        let mut study_work_isol = StudyPcts::new(
            |rep| Some(Self::calc_work_isol(rep.hashd[0].rps, *base_rps.borrow())),
            None,
        );
        let mut study_lat_impact = StudyPcts::new(
            |rep| {
                Some(Self::calc_lat_impact(
                    rep.hashd[0].lat.ctl.max(*base_lat.borrow()),
                    *base_lat.borrow(),
                ))
            },
            None,
        );
        let mut study_read_lat_pcts = StudyIoLatPcts::new("read", None);
        let mut study_write_lat_pcts = StudyIoLatPcts::new("write", None);

        let mut studies = Studies::new()
            .add(&mut study_work_isol)
            .add(&mut study_lat_impact)
            .add_multiple(&mut study_read_lat_pcts.studies())
            .add_multiple(&mut study_write_lat_pcts.studies());

        for r in results.iter() {
            base_rps.replace(r.base_rps);
            base_lat.replace(r.base_lat);

            for per in r.hog_periods.iter() {
                studies.run(rctx, *per);
            }
        }

        cmb.work_isol_pcts = study_work_isol.result(&Self::PCTS);
        cmb.lat_impact_pcts = study_lat_impact.result(&Self::PCTS);
        cmb.iolat_pcts = [
            study_read_lat_pcts.result(rctx, None),
            study_write_lat_pcts.result(rctx, None),
        ];

        cmb
    }

    fn format_params<'a>(&self, out: &mut Box<dyn Write + 'a>) {
        writeln!(
            out,
            "Params: loops={} load={} speed={}",
            self.loops, self.load, self.speed
        )
        .unwrap();
    }

    fn format_info<'a>(out: &mut Box<dyn Write + 'a>, result: &MemHogResult) {
        writeln!(
            out,
            "Info: baseline_rps={:.2}:{:.2} baseline_lat={}:{} vrate={:.2}:{:.2}",
            result.base_rps,
            result.base_rps_stdev,
            format_duration(result.base_lat),
            format_duration(result.base_lat_stdev),
            result.vrate,
            result.vrate_stdev,
        )
        .unwrap();
        writeln!(
            out,
            "      io_usage={:.1} io_unused={:.1} hog_io_usage={:.1} hog_io_loss={:.1}",
            result.io_usage, result.io_unused, result.hog_io_usage, result.hog_io_loss,
        )
        .unwrap();
        writeln!(
            out,
            "      hog_bytes={} hog_lost_bytes={}\n",
            format_size(result.hog_bytes),
            format_size(result.hog_lost_bytes)
        )
        .unwrap();
    }

    fn format_result<'a>(out: &mut Box<dyn Write + 'a>, result: &MemHogResult, full: bool) {
        if full {
            Self::format_info(out, result);

            let iolat_pcts = &result.iolat_pcts.as_ref();
            writeln!(out, "IO Latency Distribution:\n").unwrap();
            StudyIoLatPcts::format_table(out, &iolat_pcts[READ], None, "READ");
            writeln!(out, "").unwrap();
            StudyIoLatPcts::format_table(out, &iolat_pcts[WRITE], None, "WRITE");
            writeln!(out, "").unwrap();
        }

        StudyIoLatPcts::format_rw_summary(out, &result.iolat_pcts, None);

        writeln!(
            out,
            "\nWork Isolation and Request Latency Impact Distributions:\n"
        )
        .unwrap();
        writeln!(
            out,
            "           {}",
            Self::PCTS
                .iter()
                .map(|x| format!("{:>5}", format_percentile(*x)))
                .collect::<Vec<String>>()
                .join(" ")
        )
        .unwrap();

        write!(out, "Work Isol  ").unwrap();
        for pct in Self::PCTS.iter() {
            write!(out, "{:>5.2} ", result.work_isol_pcts[*pct]).unwrap();
        }
        writeln!(out, "").unwrap();

        write!(out, "Lat Impact ").unwrap();
        for pct in Self::PCTS.iter() {
            write!(out, "{:>5.2} ", result.lat_impact_pcts[*pct]).unwrap();
        }
        writeln!(out, "").unwrap();

        writeln!(
            out,
            "\nResult: work_isol={:.3}:{:.3} lat_impact={:.3}:{:.3} work_csv={:.3}",
            result.work_isol,
            result.work_isol_stdev,
            result.lat_impact,
            result.lat_impact_stdev,
            result.work_csv
        )
        .unwrap();
    }
}

#[derive(Clone, Debug)]
pub enum Scenario {
    MemHog(MemHog),
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ScenarioResult {
    MemHog(MemHogResult),
}

impl Scenario {
    fn parse(mut props: BTreeMap<String, String>) -> Result<Self> {
        match props.remove("scenario").as_deref() {
            Some("mem-hog") => {
                let mut loops = MemHog::DFL_LOOPS;
                let mut load = MemHog::DFL_LOAD;
                let mut speed = MemHogSpeed::Hog2x;
                for (k, v) in props.iter() {
                    match k.as_str() {
                        "loops" => loops = v.parse::<u32>()?,
                        "load" => load = parse_frac(v)?,
                        "speed" => speed = MemHogSpeed::from_str(&v)?,
                        k => bail!("unknown mem-hog property {:?}", k),
                    }
                    if loops == 0 {
                        bail!("\"loops\" can't be 0");
                    }
                }
                Ok(Self::MemHog(MemHog {
                    loops,
                    load,
                    speed,
                    main_period: (0, 0),
                    runs: vec![],
                }))
            }
            _ => bail!("\"scenario\" invalid or missing"),
        }
    }

    fn run(&mut self, rctx: &mut RunCtx) -> Result<ScenarioResult> {
        Ok(match self {
            Self::MemHog(mem_hog) => ScenarioResult::MemHog(mem_hog.run(rctx)?),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProtectionJob {
    pub passive: bool,
    pub balloon_size: usize,
    pub scenarios: Vec<Scenario>,
}

pub struct ProtectionBench {}

impl Bench for ProtectionBench {
    fn desc(&self) -> BenchDesc {
        BenchDesc::new("protection").takes_run_propsets()
    }

    fn parse(&self, spec: &JobSpec, _prev_data: Option<&JobData>) -> Result<Box<dyn Job>> {
        Ok(Box::new(ProtectionJob::parse(spec)?))
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProtectionResult {
    pub scenarios: Vec<ScenarioResult>,
    pub combined_mem_hog: Option<MemHogResult>,
}

impl ProtectionJob {
    pub fn parse(spec: &JobSpec) -> Result<Self> {
        let mut job = Self::default();

        for (k, v) in spec.props[0].iter() {
            match k.as_str() {
                "passive" => job.passive = v.len() == 0 || v.parse::<bool>()?,
                "balloon" => job.balloon_size = parse_size(v)? as usize,
                k => bail!("unknown property key {:?}", k),
            }
        }

        for props in spec.props[1..].iter() {
            job.scenarios.push(Scenario::parse(props.clone())?);
        }

        if job.scenarios.len() == 0 {
            info!("protection: Using default scenario set");
            job.scenarios.push(
                Scenario::parse(
                    [
                        ("scenario".to_owned(), "mem-hog".to_owned()),
                        ("load".to_owned(), "1.0".to_owned()),
                    ]
                    .iter()
                    .cloned()
                    .collect(),
                )
                .unwrap(),
            );
            job.scenarios.push(
                Scenario::parse(
                    [
                        ("scenario".to_owned(), "mem-hog".to_owned()),
                        ("load".to_owned(), "0.8".to_owned()),
                    ]
                    .iter()
                    .cloned()
                    .collect(),
                )
                .unwrap(),
            );
        }

        Ok(job)
    }

    pub fn format_result<'a>(
        &self,
        mut out: &mut Box<dyn Write + 'a>,
        result: &ProtectionResult,
        full: bool,
        prefix: &str,
    ) {
        let underline_char = match prefix.len() {
            0 => "=",
            _ => "-",
        };

        if full {
            for (idx, (scn, res)) in self
                .scenarios
                .iter()
                .zip(result.scenarios.iter())
                .enumerate()
            {
                match (scn, res) {
                    (Scenario::MemHog(mh), ScenarioResult::MemHog(mhr)) => {
                        writeln!(
                            out,
                            "\n{}",
                            custom_underline(
                                &format!(
                                    "{}Scenario {}/{} - Memory Hog",
                                    prefix,
                                    idx + 1,
                                    self.scenarios.len()
                                ),
                                underline_char
                            )
                        )
                        .unwrap();
                        mh.format_params(&mut out);
                        writeln!(out, "").unwrap();
                        MemHog::format_result(out, mhr, full);
                    }
                }
            }
        }

        if let Some(hog_result) = result.combined_mem_hog.as_ref() {
            writeln!(
                out,
                "\n{}",
                custom_underline(&format!("{}Memory Hog Summary", prefix), underline_char)
            )
            .unwrap();
            MemHog::format_result(out, hog_result, full);
        }
    }
}

impl Job for ProtectionJob {
    fn sysreqs(&self) -> BTreeSet<SysReq> {
        ALL_SYSREQS.clone()
    }

    fn run(&mut self, rctx: &mut RunCtx) -> Result<serde_json::Value> {
        rctx.maybe_run_nested_hashd_params()?;

        if self.passive {
            rctx.set_passive_keep_crit_mem_prot();
        }
        if self.balloon_size > 0 {
            rctx.set_balloon_size(self.balloon_size);
        }
        rctx.set_prep_testfiles().start_agent();

        let mut result = ProtectionResult::default();

        for scn in self.scenarios.iter_mut() {
            result.scenarios.push(scn.run(rctx)?);
        }

        let mut mhs = vec![];
        for result in result.scenarios.iter() {
            match result {
                ScenarioResult::MemHog(mh) => {
                    mhs.push(mh);
                }
            }
        }

        if mhs.len() > 0 {
            result.combined_mem_hog = Some(MemHog::combine_results(rctx, &mhs));
        }

        Ok(serde_json::to_value(&result).unwrap())
    }

    fn format<'a>(
        &self,
        mut out: Box<dyn Write + 'a>,
        data: &JobData,
        full: bool,
        _props: &JobProps,
    ) -> Result<()> {
        let result = serde_json::from_value::<ProtectionResult>(data.result.clone()).unwrap();
        self.format_result(&mut out, &result, full, "");
        Ok(())
    }
}
