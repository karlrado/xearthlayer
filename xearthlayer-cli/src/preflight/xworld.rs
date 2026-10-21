//! SimHeaven X-World Pro checks on every `run` (#213). Detection contributes
//! what X-Plane will load; the order check can stop startup; the setup check
//! only warns. XEarthLayer never edits scenery_packs.ini or X-World Pro.

use std::borrow::Cow;

use xearthlayer::preflight::{BootstrapContext, Preflight, PreflightError, Remedy, Status};
use xearthlayer::xworld_pro::{detect, order_violation, setup_findings, OverlayCoverage};

use super::names;

const ENABLE_HINT: &str = "SimHeaven X-World Pro detected. Run 'xearthlayer setup', or \
'xearthlayer config set packages.xworld_pro true', to give it priority over XEarthLayer overlays";

const ORDER_HINT: &str = "Move XEarthLayer's entries (zzXEL_..., yzXEL_...) below every \
X-World Pro layer in scenery_packs.ini, as the 'Scenery_packs.ini' section of the \
X-World Pro manual describes, then start XEarthLayer again";

fn xworld_on(ctx: &BootstrapContext) -> bool {
    ctx.command() == "run" && ctx.config().is_some_and(|c| c.packages.xworld_pro)
}

/// Contributes the X-World Pro detection when the setting is on, and hints
/// when it is off but X-World Pro is installed.
pub struct XWorldProDetect;

impl Preflight<BootstrapContext> for XWorldProDetect {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_DETECT.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(config) = ctx.config() else {
            return Status::unsatisfied("configuration has not been loaded");
        };
        let Some(custom_scenery) = ctx.custom_scenery_path() else {
            return Status::unsatisfied("Custom Scenery path has not been resolved");
        };
        if !config.packages.xworld_pro {
            return if detect(custom_scenery).is_present() {
                Status::Warning(ENABLE_HINT.to_string())
            } else {
                Status::Satisfied
            };
        }
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run").remediable();
        };
        match coverage_report(d, config.packages.disable_overlays) {
            CoverageReport::Info(message) => {
                tracing::info!("{message}");
                Status::Satisfied
            }
            CoverageReport::Warn(message) => Status::Warning(message),
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let custom_scenery = ctx.custom_scenery_path().cloned().ok_or_else(|| {
            PreflightError::new(
                names::XWORLD_PRO_DETECT,
                "Custom Scenery path has not been resolved",
            )
        })?;
        ctx.set_xworld_pro(detect(&custom_scenery));
        Ok(Remedy::default())
    }
}

/// The detection outcome as a message: a report of what is active (logged)
/// or a misconfiguration the user should see (printed as a warning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageReport {
    Info(String),
    Warn(String),
}

/// Describe what X-World Pro detection found. Active continents are a report
/// the dashboard's Status pane also shows; an unreadable file, support with
/// nothing installed, or no enabled continent are worth the user's attention.
pub fn coverage_report(
    d: &xearthlayer::xworld_pro::Detection,
    disable_overlays: bool,
) -> CoverageReport {
    let mut report = if let Some(e) = &d.read_error {
        CoverageReport::Warn(format!(
            "X-World Pro: could not read {e}; XEarthLayer overlays are unchanged"
        ))
    } else if !d.is_present() {
        CoverageReport::Warn(
            "packages.xworld_pro is on, but no X-World Pro was found in scenery_packs.ini; \
             XEarthLayer overlays are unchanged"
                .to_string(),
        )
    } else {
        let coverage = OverlayCoverage::from_detection(d);
        if coverage.is_empty() {
            CoverageReport::Warn(
                "X-World Pro is installed but no continent is enabled; XEarthLayer overlays \
                 are unchanged"
                    .to_string(),
            )
        } else if coverage.whole_planet {
            CoverageReport::Info(
                "X-World Pro: the whole planet is active; all XEarthLayer overlays are off"
                    .to_string(),
            )
        } else {
            CoverageReport::Info(format!(
                "X-World Pro: {} active; XEarthLayer overlays are off there",
                coverage.describe()
            ))
        }
    };
    if disable_overlays {
        let (CoverageReport::Info(message) | CoverageReport::Warn(message)) = &mut report;
        message.push_str(" (packages.disable_overlays turns every XEarthLayer overlay off anyway)");
    }
    report
}

/// What the user chose when the order is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderChoice {
    Stop,
    Continue,
}

/// How an order problem is put to the user. Injected so a test never asks.
pub trait ConfirmOrder: Send + Sync {
    fn decide(&self, problem: &str) -> OrderChoice;
}

/// Continue without asking: tests, and the interactive confirmer without a terminal.
pub struct ContinueAnyway;

impl ConfirmOrder for ContinueAnyway {
    fn decide(&self, _problem: &str) -> OrderChoice {
        OrderChoice::Continue
    }
}

/// Ask, when there is a terminal to ask in.
pub struct AskUserOrder;

impl ConfirmOrder for AskUserOrder {
    fn decide(&self, problem: &str) -> OrderChoice {
        use std::io::IsTerminal;
        // `cfg!(test)` backs up injection: a test must never block on input,
        // and stdin is a terminal when the suite runs from a shell.
        let can_ask = !cfg!(test) && std::io::stdin().is_terminal();
        if !can_ask {
            return ContinueAnyway.decide(problem);
        }
        println!("X-World Pro scenery order: {problem}");
        let theme = dialoguer::theme::ColorfulTheme::default();
        let choice = dialoguer::Select::with_theme(&theme)
            .with_prompt("XEarthLayer should load below X-World Pro")
            .items(&["Stop so I can fix scenery_packs.ini", "Continue anyway"])
            .default(0)
            .interact()
            .unwrap_or(1);
        if choice == 0 {
            OrderChoice::Stop
        } else {
            OrderChoice::Continue
        }
    }
}

/// XEarthLayer's entries must load below every enabled X-World Pro layer.
pub struct XWorldProOrder {
    confirm: Box<dyn ConfirmOrder>,
}

impl XWorldProOrder {
    pub fn new(confirm: Box<dyn ConfirmOrder>) -> Self {
        Self { confirm }
    }
}

impl Preflight<BootstrapContext> for XWorldProOrder {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_ORDER.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        xworld_on(ctx)
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run");
        };
        match order_violation(d) {
            None => Status::Satisfied,
            Some(v) if ctx.xworld_order_accepted() => Status::Warning(format!(
                "X-World Pro scenery order: {}; continuing as asked",
                v.describe()
            )),
            Some(v) => Status::unsatisfied(format!("X-World Pro scenery order: {}", v.describe()))
                .remediable()
                .with_hint(ORDER_HINT),
        }
    }

    fn remediate(
        &self,
        ctx: &mut BootstrapContext,
    ) -> Result<Remedy<BootstrapContext>, PreflightError> {
        let problem = ctx
            .xworld_pro()
            .and_then(order_violation)
            .map(|v| v.describe());
        if let Some(problem) = problem {
            if self.confirm.decide(&problem) == OrderChoice::Continue {
                ctx.accept_xworld_order();
            }
        }
        Ok(Remedy::default())
    }
}

/// Warnings for the ortho library, mesh variants, install links and broken entries.
pub struct XWorldProSetup;

impl Preflight<BootstrapContext> for XWorldProSetup {
    fn name(&self) -> Cow<'static, str> {
        names::XWORLD_PRO_SETUP.into()
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        xworld_on(ctx)
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(d) = ctx.xworld_pro() else {
            return Status::unsatisfied("X-World Pro detection has not run");
        };
        let findings = setup_findings(d);
        if findings.is_empty() {
            return Status::Satisfied;
        }
        Status::Warning(
            findings
                .iter()
                .map(|f| f.message())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use xearthlayer::config::ConfigFile;
    use xearthlayer::xworld_pro::SCENERY_PACKS_FILE;

    const EU09: &str = "simHeaven_X-WORLD-Pro_Europe-09-scenery";

    /// A run context whose Custom Scenery holds `folders` and `ini`.
    fn ctx(xworld_pro: bool, folders: &[&str], ini: &str) -> (tempfile::TempDir, BootstrapContext) {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("Custom Scenery");
        std::fs::create_dir_all(&cs).unwrap();
        for f in folders {
            std::fs::create_dir_all(cs.join(f)).unwrap();
        }
        std::fs::write(cs.join(SCENERY_PACKS_FILE), ini).unwrap();
        let mut config = ConfigFile::default();
        config.packages.xworld_pro = xworld_pro;
        let mut ctx = BootstrapContext::new("run", None);
        ctx.set_config(config);
        ctx.set_custom_scenery_path(cs);
        (dir, ctx)
    }

    fn pack(name: &str) -> String {
        format!("SCENERY_PACK Custom Scenery/{name}/\n")
    }

    struct Stop;
    impl ConfirmOrder for Stop {
        fn decide(&self, _: &str) -> OrderChoice {
            OrderChoice::Stop
        }
    }

    #[test]
    fn detect_only_hints_when_the_setting_is_off() {
        let (_d, c) = ctx(false, &[EU09], &pack(EU09));
        match XWorldProDetect.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("xearthlayer setup"), "{m}"),
            other => panic!("{other:?}"),
        }
        let (_d, c) = ctx(false, &[], "");
        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
    }

    #[test]
    fn detect_contributes_and_then_reports_the_continents() {
        let (_d, mut c) = ctx(true, &[EU09], &pack(EU09));
        assert!(matches!(
            XWorldProDetect.inspect(&c),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        XWorldProDetect.remediate(&mut c).unwrap();
        // Active continents are a report, not a problem: it goes to the log
        // and the dashboard's Status pane, not to the terminal.
        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
        match coverage_report(c.xworld_pro().unwrap(), false) {
            CoverageReport::Info(m) => assert!(m.contains("Europe"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_whole_planet_is_logged_not_printed() {
        let folders: Vec<String> = ["Africa", "America", "Asia", "Australia-Oceania", "Europe"]
            .iter()
            .map(|c| format!("simHeaven_X-WORLD-Pro_{c}-09-scenery"))
            .collect();
        let names: Vec<&str> = folders.iter().map(String::as_str).collect();
        let ini: String = names.iter().map(|n| pack(n)).collect();
        let (_d, mut c) = ctx(true, &names, &ini);
        XWorldProDetect.remediate(&mut c).unwrap();

        assert_eq!(XWorldProDetect.inspect(&c), Status::Satisfied);
        assert_eq!(
            coverage_report(c.xworld_pro().unwrap(), true),
            CoverageReport::Info(
                "X-World Pro: the whole planet is active; all XEarthLayer overlays are off \
                 (packages.disable_overlays turns every XEarthLayer overlay off anyway)"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_setting_with_nothing_installed_still_warns_on_the_terminal() {
        let (_d, mut c) = ctx(true, &[], "");
        XWorldProDetect.remediate(&mut c).unwrap();
        match XWorldProDetect.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("no X-World Pro was found"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn detect_names_a_missing_custom_scenery_path() {
        let mut c = BootstrapContext::new("run", None);
        c.set_config(ConfigFile::default());
        match XWorldProDetect.inspect(&c) {
            Status::Unsatisfied { reason, .. } => {
                assert!(reason.contains("Custom Scenery"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn order_and_setup_apply_only_with_the_setting_on_and_for_run() {
        let (_d, c) = ctx(false, &[], "");
        assert!(!XWorldProOrder::new(Box::new(ContinueAnyway)).applies(&c));
        assert!(!XWorldProSetup.applies(&c));
        let (_d, c) = ctx(true, &[], "");
        assert!(XWorldProOrder::new(Box::new(ContinueAnyway)).applies(&c));
        let mut s = BootstrapContext::new("setup", None);
        s.set_config(c.config().unwrap().clone());
        assert!(!XWorldProSetup.applies(&s));
    }

    fn detected(
        xworld_pro: bool,
        folders: &[&str],
        ini: &str,
    ) -> (tempfile::TempDir, BootstrapContext) {
        let (d, mut c) = ctx(xworld_pro, folders, ini);
        XWorldProDetect.remediate(&mut c).unwrap();
        (d, c)
    }

    #[test]
    fn a_good_order_is_satisfied() {
        let (_d, c) = detected(true, &[EU09], &(pack(EU09) + &pack("zzXEL_ortho")));
        assert_eq!(
            XWorldProOrder::new(Box::new(Stop)).inspect(&c),
            Status::Satisfied
        );
    }

    #[test]
    fn a_bad_order_continues_as_a_warning_when_the_user_says_so() {
        let check = XWorldProOrder::new(Box::new(ContinueAnyway));
        let (_d, mut c) = detected(true, &[EU09], &(pack("zzXEL_ortho") + &pack(EU09)));
        assert!(matches!(
            check.inspect(&c),
            Status::Unsatisfied {
                remediable: true,
                ..
            }
        ));
        check.remediate(&mut c).unwrap();
        match check.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("zzXEL_ortho") && m.contains(EU09), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_order_stays_unsatisfied_when_the_user_stops() {
        let check = XWorldProOrder::new(Box::new(Stop));
        let (_d, mut c) = detected(true, &[EU09], &(pack("zzXEL_ortho") + &pack(EU09)));
        check.remediate(&mut c).unwrap();
        match check.inspect(&c) {
            Status::Unsatisfied { hint: Some(h), .. } => {
                assert!(h.contains("scenery_packs.ini"), "{h}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn setup_findings_are_one_warning() {
        let (_d, c) = detected(true, &[], &pack(EU09)); // folder missing
        match XWorldProSetup.inspect(&c) {
            Status::Warning(m) => assert!(m.contains("does not exist"), "{m}"),
            other => panic!("{other:?}"),
        }
        let (_d, c) = detected(true, &[EU09], &pack(EU09));
        assert_eq!(XWorldProSetup.inspect(&c), Status::Satisfied);
    }

    #[test]
    fn the_interactive_confirmer_never_blocks_under_test() {
        assert_eq!(AskUserOrder.decide("anything"), OrderChoice::Continue);
    }
}
