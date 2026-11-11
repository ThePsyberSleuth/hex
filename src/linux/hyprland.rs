//! Hyprland takes every global shortcut without a key, so Hex's portal
//! grants arrive unbound and nothing happens until something binds each
//! global id to a key. This module does that binding itself, through
//! `hyprctl`, and only ever touches keys no other binding holds.
//!
//! A global bind's `arg` is an index rather than the global id, so the id
//! goes in the bind's description instead; that and the `HL.Keybind` handle
//! Hex keeps are the only things that identify Hex's own binds. Removal goes
//! through those handles, never `hl.unbind`, which is combo-wide and would
//! take someone else's binding on the same keys with it.

use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::linux::input::LinuxHotkey;

/// The description Hex's binds start with; what `binds -j` shows of them.
const DESCRIPTION: &str = "Hex shortcut: ";

/// Hyprland's modifier bits.
const SHIFT: u32 = 1;
const CONTROL: u32 = 4;
const ALT: u32 = 8;
const SUPER: u32 = 64;

/// Whether the running desktop is Hyprland, which binds nothing for Hex.
pub(crate) fn running() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|signature| !signature.is_empty())
}

/// How long `hyprctl` gets before Hex treats the compositor as gone: a hung
/// session must not stop the listener answering keys.
const IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn hyprctl(arguments: &[&str]) -> Result<String> {
    run(std::path::Path::new("hyprctl"), arguments, IPC_TIMEOUT)
}

/// Runs `program`, killing it if it takes longer than `timeout`: a `hyprctl`
/// left running would still apply its `eval` after Hex gave up on it, and one
/// per retry would pile up.
fn run(
    program: &std::path::Path,
    arguments: &[&str],
    timeout: std::time::Duration,
) -> Result<String> {
    let mut child = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .wrap_err("could not run hyprctl")?;
    // Drained on their own threads, so a reply longer than a pipe buffer
    // can't wedge the wait below.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(eyre!(
                    "hyprctl {} did not answer in {timeout:?}",
                    arguments.join(" ")
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(error) => return Err(error).wrap_err("could not wait for hyprctl"),
        }
    };
    let stdout = String::from_utf8_lossy(&stdout.join().unwrap_or_default())
        .trim()
        .to_owned();
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap_or_default())
        .trim()
        .to_owned();
    if !status.success() {
        return Err(eyre!("hyprctl {} failed: {stderr}", arguments.join(" ")));
    }
    Ok(stdout)
}

fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// A binding as `hyprctl -j binds` prints it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Bind {
    pub modmask: u32,
    pub key: String,
    pub dispatcher: String,
    pub submap: String,
    pub description: String,
}

impl Bind {
    fn is_global(&self) -> bool {
        self.dispatcher == "HL.Dispatcher(global)"
    }

    /// Whether this is the bind Hex made for `global`.
    fn is_hex(&self, global: &str) -> bool {
        self.submap.is_empty()
            && self.is_global()
            && self.description == format!("{DESCRIPTION}{global}")
    }

    /// The combination as `hl.bind` writes it. `self.key` comes back from
    /// Hyprland already named, so it goes in verbatim; `None` when it can't go
    /// inside a Lua string.
    fn spec(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (held, name) in [
            (self.modmask & SUPER != 0, "SUPER"),
            (self.modmask & CONTROL != 0, "CTRL"),
            (self.modmask & ALT != 0, "ALT"),
            (self.modmask & SHIFT != 0, "SHIFT"),
        ] {
            if held {
                parts.push(name.to_owned());
            }
        }
        if !nameable(&self.key.to_ascii_uppercase()) {
            return None;
        }
        parts.push(self.key.clone());
        Some(parts.join(" + "))
    }
}

fn binds() -> Result<Vec<Bind>> {
    let text = hyprctl(&["-j", "binds"])?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .wrap_err_with(|| format!("could not read Hyprland's bindings: {text}"))?;
    let Some(bindings) = parsed.as_array() else {
        return Err(eyre!("Hyprland's bindings weren't a list: {text}"));
    };
    Ok(bindings
        .iter()
        .map(|bind| Bind {
            modmask: bind["modmask"].as_u64().unwrap_or_default() as u32,
            key: bind["key"].as_str().unwrap_or_default().to_owned(),
            dispatcher: bind["dispatcher"].as_str().unwrap_or_default().to_owned(),
            submap: bind["submap"].as_str().unwrap_or_default().to_owned(),
            description: bind["description"].as_str().unwrap_or_default().to_owned(),
        })
        .collect())
}

/// One key combination, as Hex's settings record it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Combo {
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}

impl Combo {
    fn mask(&self) -> u32 {
        u32::from(self.shift) * SHIFT
            + u32::from(self.control) * CONTROL
            + u32::from(self.alt) * ALT
            + u32::from(self.super_key) * SUPER
    }

    /// Hyprland's way of writing the combination: `ALT + SPACE`. `None` for a
    /// key Hyprland has no name for, which would bind nothing.
    fn spec(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (held, name) in [
            (self.super_key, "SUPER"),
            (self.control, "CTRL"),
            (self.alt, "ALT"),
            (self.shift, "SHIFT"),
        ] {
            if held {
                parts.push(name.to_owned());
            }
        }
        let key = hypr_key(&self.key);
        if !nameable(&key) {
            return None;
        }
        parts.push(key);
        Some(parts.join(" + "))
    }

    fn holds(&self, bind: &Bind) -> bool {
        bind.submap.is_empty()
            && bind.modmask == self.mask()
            && bind.key.eq_ignore_ascii_case(&hypr_key(&self.key))
    }
}

impl From<&LinuxHotkey> for Combo {
    fn from(key: &LinuxHotkey) -> Self {
        Self {
            alt: key.alt,
            control: key.control,
            shift: key.shift,
            super_key: key.super_key,
            key: key.key.clone(),
        }
    }
}

/// The key as Hyprland names it: `SPACE`, `F10`, `V`, `COMMA`.
fn hypr_key(key: &str) -> String {
    let key = key.to_ascii_lowercase();
    match key.as_str() {
        "space" | " " => "SPACE".into(),
        "enter" | "return" => "RETURN".into(),
        "tab" => "TAB".into(),
        "backspace" => "BACKSPACE".into(),
        "-" | "minus" => "MINUS".into(),
        "=" | "equal" => "EQUAL".into(),
        "[" | "bracketleft" => "BRACKETLEFT".into(),
        "]" | "bracketright" => "BRACKETRIGHT".into(),
        ";" | "semicolon" => "SEMICOLON".into(),
        "'" | "apostrophe" => "APOSTROPHE".into(),
        "`" | "grave" => "GRAVE".into(),
        "\\" | "backslash" => "BACKSLASH".into(),
        "," | "comma" => "COMMA".into(),
        "." | "period" => "PERIOD".into(),
        "/" | "slash" => "SLASH".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// A key name safe inside the Lua string Hex sends; `str::contains` over the
/// xkb punctuation names, which are all letters.
fn nameable(key: &str) -> bool {
    !key.is_empty()
        && (key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
            || [
                "COMMA",
                "MINUS",
                "EQUAL",
                "BRACKETLEFT",
                "BRACKETRIGHT",
                "SEMICOLON",
                "APOSTROPHE",
                "GRAVE",
                "BACKSLASH",
                "PERIOD",
                "SLASH",
            ]
            .contains(&key))
}

/// A shortcut Hex wants reachable: its global id on its combination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Wanted {
    pub combo: Combo,
    pub global: String,
}

/// The global ids Hyprland should answer, on the keys Settings records.
/// Hyprland's portal namespaces each id Hex binds (`anomaly.hex:dictation`),
/// and its global dispatcher needs that full name.
pub(crate) fn wanted(
    requested: &[Option<(crate::linux::input::Shortcut, &LinuxHotkey)>],
    ids: &[Option<String>; crate::linux::input::Shortcut::ALL.len()],
) -> Vec<Wanted> {
    let mut wanted: Vec<Wanted> = Vec::new();
    for (shortcut, key) in requested.iter().flatten() {
        let Some(id) = ids[shortcut.index()].as_deref() else {
            continue;
        };
        let global = format!("{}:{id}", crate::linux::portal::APP_ID);
        let combo = Combo::from(*key);
        // Two shortcuts on one key can't both be bound; the first keeps it.
        if wanted
            .iter()
            .any(|wanted| wanted.combo == combo || wanted.global == global)
        {
            continue;
        }
        wanted.push(Wanted { combo, global });
    }
    wanted
}

/// What applying [`Wanted`] to Hyprland's bindings would take.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct Plan {
    pub bind: Vec<Wanted>,
    /// Ids whose bind Hex made and must take out: no longer wanted, or moved
    /// to another combination.
    pub remove: Vec<String>,
    /// Combinations to unbind before binding, left behind by an earlier run
    /// whose handle is gone.
    pub clear: Vec<String>,
    /// Combination, and who holds it; Hex leaves those alone.
    pub conflicts: Vec<String>,
}

/// What Hyprland needs for `desired`, given its `existing` binds and the
/// binds Hex made before (`made`: id and the combination it went on).
pub(crate) fn plan(desired: &[Wanted], existing: &[Bind], made: &[(String, Combo)]) -> Plan {
    let mut plan = Plan::default();
    let mut remove = |global: &str| {
        if !plan.remove.iter().any(|gone| gone == global) {
            plan.remove.push(global.to_owned());
        }
    };
    for wanted in desired {
        // A settings change moves a shortcut to another key; its old bind has
        // to go even when the new key turns out to be taken.
        if made
            .iter()
            .any(|(global, combo)| global == &wanted.global && combo != &wanted.combo)
        {
            remove(&wanted.global);
        }
        let Some(spec) = wanted.combo.spec() else {
            plan.conflicts.push(format!(
                "{} is not a key Hyprland has a name for, so Hex can't bind it",
                wanted.combo.key
            ));
            continue;
        };
        let holders = existing
            .iter()
            .filter(|bind| wanted.combo.holds(bind) && !bind.is_hex(&wanted.global))
            .collect::<Vec<_>>();
        if !holders.is_empty() {
            for bind in holders {
                plan.conflicts.push(format!(
                    "{spec} is taken by {}",
                    if bind.description.is_empty() {
                        bind.dispatcher.as_str()
                    } else {
                        bind.description.as_str()
                    }
                ));
            }
            continue;
        }
        // A bind an earlier run of Hex made answers too, and its handle died
        // with that run, so only the combo names it. Clearing it is
        // combo-wide, so it is left alone if anything else holds it.
        let mut clearable = Vec::new();
        let mut stale_conflict = false;
        // Live: hl.unbind takes every bind on the combination, and Hyprland
        // stores COMMA and comma as separate binds on one key.
        for stale in existing.iter().filter(|bind| {
            bind.is_hex(&wanted.global)
                && !made
                    .iter()
                    .any(|(global, combo)| global == &wanted.global && combo.holds(bind))
        }) {
            let Some(combo) = stale.spec() else { continue };
            let shared = existing.iter().any(|other| {
                !other.is_hex(&wanted.global)
                    && other.submap.is_empty()
                    && other.modmask == stale.modmask
                    && other.key.eq_ignore_ascii_case(&stale.key)
            });
            if shared {
                plan.conflicts.push(format!(
                    "{combo} still holds a Hex shortcut from an earlier run"
                ));
                clearable.clear();
                stale_conflict = true;
                break;
            }
            clearable.push(combo);
        }
        if stale_conflict {
            continue;
        }
        plan.clear.extend(clearable.iter().cloned());
        let bound = existing.iter().any(|bind| {
            wanted.combo.holds(bind)
                && bind.is_hex(&wanted.global)
                && !clearable
                    .iter()
                    .any(|combo| bind.spec().as_deref() == Some(combo.as_str()))
        });
        let current = made
            .iter()
            .any(|(global, combo)| global == &wanted.global && combo == &wanted.combo);
        // Binding again would leave Hyprland two binds on the key, and the
        // old one would answer first.
        if !bound || !current {
            plan.bind.push(wanted.clone());
        }
    }
    for (global, _) in made {
        if !desired.iter().any(|wanted| &wanted.global == global) {
            remove(global);
        }
    }
    plan
}

fn eval(code: &str) -> Result<()> {
    let reply = hyprctl(&["eval", code])?;
    if reply.starts_with("error") || reply.contains("error:") {
        return Err(eyre!("Hyprland refused `{code}`: {reply}"));
    }
    Ok(())
}

/// The binds Hex made, so a later run binds no duplicates and removes only
/// its own.
#[derive(Debug, Default)]
pub(crate) struct Binds {
    made: Vec<(String, Combo)>,
    notes: Vec<String>,
}

impl Drop for Binds {
    /// Every way out of the listener takes Hex's binds back out, including an
    /// error: a compositor key left grabbed with Hex gone would do nothing.
    fn drop(&mut self) {
        self.stop();
    }
}

impl Binds {
    /// Makes Hyprland's bindings carry `desired`, returning what it couldn't:
    /// conflicts with other bindings, and failures to bind.
    pub(crate) fn sync(&mut self, desired: &[Wanted]) -> Vec<String> {
        let existing = match binds() {
            Ok(existing) => existing,
            Err(error) => return self.changed(vec![format!("{error:#}")]),
        };
        let plan = plan(desired, &existing, &self.made);
        let mut notes = plan.conflicts;
        // A bind left on the combo by an earlier run answered as well as the
        // one about to be made, so it goes first.
        for combo in plan.clear {
            if let Err(error) = eval(&unbind_lua(&combo)) {
                notes.push(format!("{error:#}"));
            }
        }
        // A shortcut whose old bind Hex couldn't take out stays where it is:
        // binding it on a second key as well would answer twice.
        let mut stuck = Vec::new();
        for global in plan.remove {
            match eval(&remove_lua(&global)) {
                Ok(()) => self.made.retain(|(made, _)| made != &global),
                Err(error) => {
                    notes.push(format!("{error:#}"));
                    stuck.push(global);
                }
            }
        }
        for wanted in plan.bind {
            if stuck.contains(&wanted.global) {
                continue;
            }
            // A `hyprctl` that timed out may still have bound the key, so the
            // bind is recorded either way: the next plan sees whether it
            // reached Hyprland's bindings and binds it again if it didn't,
            // and `_hex[global]` is there to remove it if it did.
            self.made.retain(|(made, _)| made != &wanted.global);
            self.made
                .push((wanted.global.clone(), wanted.combo.clone()));
            if let Err(error) = eval(&bind_lua(&wanted)) {
                notes.push(format!("{error:#}"));
            }
        }
        self.changed(notes)
    }

    /// Notes only when they differ from last time: a conflict that keeps
    /// holding is logged once, not every resync.
    fn changed(&mut self, notes: Vec<String>) -> Vec<String> {
        if notes == self.notes {
            return Vec::new();
        }
        self.notes = notes.clone();
        notes
    }

    /// Takes Hex's binds back out, so the keys work as they did before.
    pub(crate) fn stop(&mut self) {
        self.notes.clear();
        for (global, _) in std::mem::take(&mut self.made) {
            if let Err(error) = eval(&remove_lua(&global)) {
                tracing::warn!(
                    error = format!("{error:#}"),
                    shortcut = %global,
                    "could not take a Hex shortcut off Hyprland"
                );
            }
        }
    }
}

/// Binds `global` on its combination, keeping the handle where
/// [`remove_lua`] finds it. `hyprctl eval` keeps Lua globals for the
/// session, so the handle outlives this call.
fn bind_lua(wanted: &Wanted) -> String {
    // `plan` only binds combinations `spec` names, so this is never absent.
    let spec = wanted
        .combo
        .spec()
        .expect("a planned bind always has a named key");
    let global = &wanted.global;
    format!(
        "_hex = _hex or {{}} \
         if _hex[\"{global}\"] then _hex[\"{global}\"]:remove() end \
         _hex[\"{global}\"] = hl.bind(\"{spec}\", hl.dsp.global(\"{global}\"), \
         {{description = \"{DESCRIPTION}{global}\"}})"
    )
}

/// `hl.unbind` is combo-wide, so callers must have proved Hex is the combo's
/// only holder.
fn unbind_lua(spec: &str) -> String {
    format!("hl.unbind(\"{spec}\")")
}

/// Removes the bind Hex made for `global`, and only that bind: `hl.unbind`
/// takes every binding on the keys, including someone else's.
fn remove_lua(global: &str) -> String {
    format!(
        "_hex = _hex or {{}} \
         if _hex[\"{global}\"] then _hex[\"{global}\"]:remove() _hex[\"{global}\"] = nil end"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(alt: bool, key: &str) -> Combo {
        Combo {
            alt,
            control: false,
            shift: false,
            super_key: false,
            key: key.into(),
        }
    }

    fn bind(modmask: u32, key: &str, dispatcher: &str, description: &str) -> Bind {
        Bind {
            modmask,
            key: key.into(),
            dispatcher: dispatcher.into(),
            submap: String::new(),
            description: description.into(),
        }
    }

    fn dictation(key: &str) -> Wanted {
        Wanted {
            combo: combo(true, key),
            global: "anomaly.hex:dictation".into(),
        }
    }

    fn made(wanted: &Wanted) -> Vec<(String, Combo)> {
        vec![(wanted.global.clone(), wanted.combo.clone())]
    }

    #[test]
    fn hyprland_modifier_bits_match_its_bindings() {
        let all = Combo {
            alt: true,
            control: true,
            shift: true,
            super_key: true,
            key: "space".into(),
        };
        assert_eq!(all.mask(), 1 + 4 + 8 + 64);
        assert_eq!(combo(true, "space").mask(), 8);
        assert_eq!(
            all.spec().as_deref(),
            Some("SUPER + CTRL + ALT + SHIFT + SPACE")
        );
    }

    #[test]
    fn an_unheld_combo_is_bound() {
        let wanted = dictation("space");
        let plan = plan(
            std::slice::from_ref(&wanted),
            &[bind(0, "PRINT", "HL.Dispatcher(exec_cmd)", "")],
            &[],
        );
        assert_eq!(plan.bind, [wanted]);
        assert!(plan.remove.is_empty());
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn another_bindings_combo_is_a_conflict_and_is_left_alone() {
        let plan = plan(
            &[dictation("space")],
            &[bind(8, "SPACE", "HL.Dispatcher(exec_cmd)", "toggle")],
            &[],
        );
        assert!(plan.bind.is_empty());
        assert!(plan.remove.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
        assert!(plan.conflicts[0].contains("ALT + SPACE"));
        assert!(plan.conflicts[0].contains("toggle"));
    }

    #[test]
    fn another_apps_global_bind_on_the_combo_is_a_conflict() {
        let plan = plan(
            &[dictation("space")],
            &[bind(8, "SPACE", "HL.Dispatcher(global)", "Mute")],
            &[],
        );
        assert!(plan.bind.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn another_shortcuts_own_bind_on_the_combo_is_a_conflict() {
        let existing = [bind(
            8,
            "SPACE",
            "HL.Dispatcher(global)",
            "Hex shortcut: anomaly.hex:paste-last",
        )];
        let plan = plan(&[dictation("space")], &existing, &[]);
        assert!(plan.bind.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn a_bind_hex_made_that_is_still_right_is_left_alone() {
        let wanted = dictation("space");
        let existing = [bind(
            8,
            "SPACE",
            "HL.Dispatcher(global)",
            "Hex shortcut: anomaly.hex:dictation",
        )];
        let plan = plan(std::slice::from_ref(&wanted), &existing, &made(&wanted));
        assert!(plan.bind.is_empty() && plan.remove.is_empty() && plan.conflicts.is_empty());
    }

    #[test]
    fn a_bind_hex_made_before_a_config_reload_is_bound_again() {
        // Hyprland drops binds made over IPC when its config reloads.
        let wanted = dictation("space");
        let plan = plan(std::slice::from_ref(&wanted), &[], &made(&wanted));
        assert_eq!(plan.bind, [wanted]);
    }

    #[test]
    fn a_settings_change_moves_the_bind_off_the_old_key() {
        let wanted = Wanted {
            global: "anomaly.hex:dictation".into(),
            combo: Combo {
                alt: true,
                control: false,
                shift: true,
                super_key: false,
                key: "space".into(),
            },
        };
        let plan = plan(
            std::slice::from_ref(&wanted),
            &[],
            &[(wanted.global.clone(), combo(true, "space"))],
        );
        assert_eq!(plan.remove, ["anomaly.hex:dictation"]);
        assert_eq!(plan.bind, [wanted]);
    }

    #[test]
    fn a_settings_change_off_frees_the_old_key_even_when_the_new_one_is_taken() {
        let wanted = dictation("z");
        let existing = [bind(8, "Z", "HL.Dispatcher(exec_cmd)", "something else")];
        let plan = plan(
            &[wanted],
            &existing,
            &[("anomaly.hex:dictation".into(), combo(true, "space"))],
        );
        assert_eq!(plan.remove, ["anomaly.hex:dictation"]);
        assert!(plan.bind.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn a_shortcut_turned_off_is_taken_out() {
        let plan = plan(
            &[],
            &[],
            &[("anomaly.hex:dictation".into(), combo(true, "space"))],
        );
        assert_eq!(plan.remove, ["anomaly.hex:dictation"]);
        assert!(plan.bind.is_empty());
    }

    #[test]
    fn removal_goes_through_the_handle_not_the_combo() {
        let remove = remove_lua("anomaly.hex:dictation");
        assert!(remove.contains(":remove()"));
        assert!(!remove.contains("unbind"));
        let bind = bind_lua(&dictation("space"));
        assert!(bind.contains("hl.bind(\"ALT + SPACE\", hl.dsp.global(\"anomaly.hex:dictation\")"));
        assert!(bind.contains("Hex shortcut: anomaly.hex:dictation"));
    }

    #[test]
    fn keys_are_named_as_hyprland_names_them() {
        assert_eq!(combo(true, "f10").spec().as_deref(), Some("ALT + F10"));
        assert_eq!(combo(true, "space").spec().as_deref(), Some("ALT + SPACE"));
        assert_eq!(hypr_key("v"), "V");
    }

    #[test]
    fn a_key_hyprland_has_no_name_for_is_a_conflict() {
        let plan = plan(&[dictation("semi;colon")], &[], &[]);
        assert!(plan.bind.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn a_restart_does_not_bind_beside_the_bind_it_left_behind() {
        let wanted = dictation("space");
        let existing = [bind(
            8,
            "SPACE",
            "HL.Dispatcher(global)",
            "Hex shortcut: anomaly.hex:dictation",
        )];
        let plan = plan(std::slice::from_ref(&wanted), &existing, &[]);
        assert_eq!(plan.clear, ["ALT + SPACE"]);
        assert_eq!(plan.bind, [wanted]);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn a_restart_clears_the_old_combo_when_the_key_changed_meanwhile() {
        let wanted = dictation("j");
        let existing = [bind(
            8,
            "SPACE",
            "HL.Dispatcher(global)",
            "Hex shortcut: anomaly.hex:dictation",
        )];
        let plan = plan(std::slice::from_ref(&wanted), &existing, &[]);
        assert_eq!(plan.clear, ["ALT + SPACE"]);
        assert_eq!(plan.bind, [wanted]);
    }

    #[test]
    fn a_bind_from_an_earlier_run_beside_someone_elses_is_a_conflict() {
        let existing = [
            bind(
                8,
                "SPACE",
                "HL.Dispatcher(global)",
                "Hex shortcut: anomaly.hex:dictation",
            ),
            bind(8, "SPACE", "HL.Dispatcher(exec_cmd)", "toggle"),
        ];
        let plan = plan(&[dictation("space")], &existing, &[]);
        assert!(plan.bind.is_empty());
        assert!(plan.clear.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn a_bind_spelled_in_another_case_still_blocks_the_clear() {
        let existing = [
            bind(
                8,
                "comma",
                "HL.Dispatcher(global)",
                "Hex shortcut: anomaly.hex:dictation",
            ),
            bind(8, "COMMA", "HL.Dispatcher(exec_cmd)", "someone else"),
        ];
        let plan = plan(&[dictation("comma")], &existing, &[]);
        assert!(plan.clear.is_empty());
        assert!(plan.bind.is_empty());
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn punctuation_keys_are_named_as_hyprland_names_them() {
        assert_eq!(combo(true, "comma").spec().as_deref(), Some("ALT + COMMA"));
        assert_eq!(combo(true, ",").spec().as_deref(), Some("ALT + COMMA"));
        assert!(combo(true, "semi;colon").spec().is_none());
    }

    #[test]
    fn a_bind_from_an_earlier_run_names_its_combo() {
        let stale = bind(8 + 4, "comma", "HL.Dispatcher(global)", "Hex shortcut: x");
        assert_eq!(stale.spec().as_deref(), Some("CTRL + ALT + comma"));
    }

    /// A stand-in for `hyprctl`, so the timeout and long replies are checked
    /// without a compositor.
    fn fake_hyprctl(body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "hex-fake-hyprctl-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_hanging_hyprctl_is_killed_rather_than_left_to_finish() {
        let script = fake_hyprctl("sleep 30");
        let started = std::time::Instant::now();
        let error = run(
            &script,
            &["eval", "1"],
            std::time::Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("did not answer"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn a_reply_longer_than_a_pipe_buffer_is_read_whole() {
        let script = fake_hyprctl("head -c 200000 /dev/zero | tr '\\0' x");
        let reply = run(
            &script,
            &["-j", "binds"],
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(reply.len(), 200_000);
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn a_failed_hyprctl_reports_what_it_said() {
        let script = fake_hyprctl("echo 'no socket' >&2; exit 1");
        let error = run(&script, &["eval", "1"], std::time::Duration::from_secs(20)).unwrap_err();
        assert!(format!("{error:#}").contains("no socket"));
        let _ = std::fs::remove_file(script);
    }
}
