//! Advisory notes: the guesswork vm keeps out of its rules.
//!
//! Two of these are about a command's *form* (see [`super::host::build_argv`]).
//! The arity rule decides behavior by counting arguments, never by inspecting
//! them: content is undecidable — the `|` in `grep 'a|b' f` is a regex, the one
//! in `echo hi | wc` is a pipe, and the same bytes cannot be told apart. So the
//! rule stays deterministic, and the guesswork is confined to *here*, where a
//! wrong guess costs one line of stderr instead of running the wrong command.
//! The third ([`unsynced_env_note`]) is about the sync's *scope*, and fires only
//! after a command has already failed. The fourth ([`stdin_note`]) is about vm's
//! own stdin, which never travels to the guest: a caller who piped data into vm
//! is told up front, instead of finding an empty output file and no error.
//!
//! One budget governs all of them: a note that fires on healthy commands trains
//! its reader (agent or human) to ignore every note, taking the real ones down
//! with it. Every check here is therefore biased hard toward silence — a missed
//! note leaves the reader exactly where they were, while a false one costs the
//! channel. Nothing else fires.
//!
//! The fifth pair ([`path_searched`], [`half_posix_path_note`]) pays no premium
//! into that budget at all: it speaks only once a spawn has already come back
//! `NotFound`, where the run is over and stderr is the whole product. The first
//! of the two is not even guesswork — the PATH is a fact vm holds and the reader
//! does not.

/// Shell operators that are never a command's own argument. No flag takes `&&`
/// or `2>&1` as its value, so seeing one in exec form is a mistake regardless
/// of where it sits.
const ALWAYS_OPERATORS: &[&str] = &["&&", "||", ">>", "<<", "2>", "2>>", "2>&1"];

/// Operators that are *also* plausible flag values — `awk -F '|'`, `cut -d '|'`,
/// `tr -d '&'` — so they only count when they do not sit in a value position
/// (see [`operators_in_exec_form`]). Deliberately absent: `;`, which `find …
/// -exec … {} ';'` passes legitimately in a non-value position, and which is
/// therefore not separable from its correct use at all.
const FLAG_VALUE_OPERATORS: &[&str] = &["|", "&", ">", "<"];

/// Continuation indent, aligning a note's later lines under the first one's
/// text (past the `vm ▸ note: ` the caller prints).
const INDENT: &str = "\n           ";

/// Notes to print (as `vm ▸ note: …`) before running `cmd` against `target`.
/// `is_file` answers whether a path names an existing file — injected so the
/// rules are testable without a filesystem; production passes a host-CWD probe,
/// which is the directory the guest checkout mirrors.
///
/// `verb` is the command the caller actually typed (`exec` or `run`), because
/// every note here ends in a corrected command line to paste — and advice that
/// silently swapped the verb would be advice for a command they did not run.
pub fn advisories(
    verb: &str,
    target: &str,
    cmd: &[String],
    is_file: impl Fn(&str) -> bool,
) -> Vec<String> {
    // Arity-exclusive by construction: a command is one form or the other, so
    // at most one of these can ever produce a note.
    match cmd {
        [script] => shell_form_note(verb, target, script, is_file)
            .into_iter()
            .collect(),
        _ => exec_form_note(verb, target, cmd).into_iter().collect(),
    }
}

/// The note for a command that *failed* in a guest whose checkout never received
/// the repo's gitignored env files (`unsynced`, as found by
/// `super::host::unsynced_env_files`). `None` when there are none — the ordinary
/// case, and the silent one.
///
/// This is the one note that fires *after* the fact, because that is where it
/// pays: the sync worked, the breadcrumbs are green, and the guest is failing on
/// a variable the host has and it does not. Without this line the reader — very
/// often an agent — goes hunting through the build, the guest env and the
/// toolchain, and finds nothing wrong there, because nothing is.
///
/// It names flags rather than a whole command line (the way the form notes do):
/// `vm claude` fails through this same path, and a note that told it to run
/// `vm exec` would be advice for a command the caller did not run. Both flags
/// exist on both verbs. `-e` comes first — it is the one fix that keeps a secret
/// off the guest's disk.
pub fn unsynced_env_note(unsynced: &[String]) -> Option<String> {
    let [first, ..] = unsynced else {
        return None;
    };
    let (subject, them) = match unsynced {
        [one] => (format!("`{one}` is"), "it"),
        many => (format!("{} are", backticked(many)), "them"),
    };
    Some(format!(
        "{subject} gitignored, so the sync left {them} on the host — if the guest needed \
         what is in {them}, that is the first thing to rule out.\
         {INDENT}Pass the values with `-e NAME=value` (they never reach the guest's disk), \
         or sync the file itself with `--with-file {first}`."
    ))
}

/// What the caller wired into vm's own stdin, as classified by
/// `super::host::stdin_source` (injected here so the wording is testable
/// without rewiring the test process's fd 0).
pub enum StdinSource {
    /// fd 0 is a pipe: `echo hi | vm exec …`.
    Piped,
    /// fd 0 is a regular file: `vm exec … < data.txt`.
    Redirected,
}

/// The note for input wired into vm — a pipe or a redirected file on vm's own
/// stdin. vm never reads it: the host↔agent pipe carries liveness, not data,
/// and the guest command's stdin is the null device (see `exec/guest.rs`). So
/// `echo hi | vm exec lin -- 'cat > f'` exits 0 having written an *empty* file
/// — no error anywhere, which is exactly the silent near-miss the advisory
/// channel exists for. It fires before the run, not after a failure, because
/// the run usually *succeeds*; a failure-gated note would never print.
///
/// The silence budget holds because a terminal and the null device are both
/// character devices, which classify as `None`: a shell, an agent harness, CI,
/// and cron — the places vm actually runs — all leave fd 0 that way. The one
/// known misfire is `cmd | while read x; do vm exec …; done`, where the loop's
/// own pipe is still on vm's fd 0; the note's *statement* stays true there (vm
/// did not read it — unlike ssh, it never eats a loop's input), only its advice
/// is moot. Accepted: that shape is rare, and suppressing it would need to read
/// the caller's mind.
pub fn stdin_note(source: Option<StdinSource>) -> Option<String> {
    let what = match source? {
        StdinSource::Piped => "piped into vm",
        StdinSource::Redirected => "redirected into vm from a file",
    };
    Some(format!(
        "input was {what}, but stdin does not travel to the guest — the command runs \
         without it.\
         {INDENT}Put the data in a file inside the repo instead: the sync carries it into \
         the checkout the command runs in."
    ))
}

/// Exec form carrying what looks like shell syntax: `vm exec lin -- echo a && echo b`
/// reaches vm as five arguments (the *host* shell already split it, and `&&`
/// survived only because it was quoted or escaped), so `&&` is handed to `echo`
/// as a literal word. Nothing fails — `echo` cheerfully prints it — which is
/// exactly why it is worth a note: the command "works", just not as meant.
fn exec_form_note(verb: &str, target: &str, cmd: &[String]) -> Option<String> {
    let ops = operators_in_exec_form(cmd);
    if ops.is_empty() {
        return None;
    }
    let (list, prog) = (backticked(&ops), &cmd[0]);
    let script = cmd.join(" ");
    Some(format!(
        "{list} reached vm as {} own argument, so it is passed to `{prog}` as a literal \
         word — several arguments run exactly as given, never as shell syntax.\
         {INDENT}To run this as a shell script, pass it as ONE argument: \
         vm {verb} {target} -- '{script}'",
        if ops.len() == 1 { "its" } else { "their" },
    ))
}

/// The operators standing on their own in an exec-form argv, in the order they
/// appear, deduplicated (one note per run, listing each operator once).
fn operators_in_exec_form(cmd: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (i, arg) in cmd.iter().enumerate() {
        // `awk -F '|' file`: an operator right after a flag is that flag's
        // value. Suppressing this costs the note in `echo --foo | wc` too —
        // accepted: a missed note is silence, a false one is noise.
        let in_value_position = i
            .checked_sub(1)
            .and_then(|prev| cmd.get(prev))
            .is_some_and(|prev| prev.starts_with('-'));
        let is_operator = ALWAYS_OPERATORS.contains(&arg.as_str())
            || (!in_value_position && FLAG_VALUE_OPERATORS.contains(&arg.as_str()));
        if is_operator && !found.contains(arg) {
            found.push(arg.clone());
        }
    }
    found
}

/// Shell form whose script *starts with the name of a real file that has a
/// space in it*: `vm exec mac -- '/Applications/My App/run --flag'`. One
/// argument is a script, so the guest shell word-splits it and tries to run
/// `/Applications/My` — the one case where the arity rule's convenience bites,
/// and the reason it is checked against the filesystem rather than guessed at:
/// only an actual file on disk earns the note.
fn shell_form_note(
    verb: &str,
    target: &str,
    script: &str,
    is_file: impl Fn(&str) -> bool,
) -> Option<String> {
    let end = spaced_file_prefix(script, &is_file)?;
    let (path, rest) = (&script[..end], &script[end..]);
    Some(format!(
        "`{path}` is an existing file whose name holds a space, but a single argument runs \
         as a shell script — the guest shell splits it there and never sees that file.\
         {INDENT}To run it, quote the path for the shell: vm {verb} {target} -- '\"{path}\"{rest}'"
    ))
}

/// The end of the shortest multi-word prefix of `script` that names a file, if
/// any. Multi-word only: a one-word prefix has no space, so no shell can split
/// it — `'run.sh --flag'` is fine and must stay silent.
fn spaced_file_prefix(script: &str, is_file: &impl Fn(&str) -> bool) -> Option<usize> {
    let mut word_ends = Vec::new();
    let mut in_word = false;
    for (i, c) in script.char_indices() {
        match (c.is_whitespace(), in_word) {
            (true, true) => {
                word_ends.push(i);
                in_word = false;
            }
            (false, _) => in_word = true,
            _ => {}
        }
    }
    if in_word {
        word_ends.push(script.len());
    }
    word_ends.into_iter().skip(1).find(|&end| {
        let candidate = &script[..end];
        // A Windows-style relative path (`scripts\my script.sh`) still names a
        // host file once separators are normalized — the host is where we look.
        is_file(candidate) || is_file(&candidate.replace('\\', "/"))
    })
}

/// How much of one PATH entry prints before it is elided. A half-converted PATH
/// (see [`half_posix_path_note`]) packs its entire POSIX tail into a *single*
/// entry — kilobytes of it — and what the reader needs from that entry is its
/// shape, not its forty-fourth path. Never a silent cut: the count on the line
/// below it is of the whole thing.
const ENTRY_WIDTH: usize = 76;

/// The PATH vm was about to search, printed after a spawn came back with the one
/// failure a PATH fully explains: command not found.
///
/// This is the piece of state the reader cannot see and vm can. It is not the
/// PATH of the shell they are standing in: a guest command searches the agent's
/// augmented PATH, `--or-native` on a CI runner searches whatever that runner
/// exported, and `-e PATH=…` overrides either. Printing it is what makes a
/// not-found a one-minute diagnosis instead of three CI rounds (#25).
///
/// Entries no resolver can search are marked `⚠` — see [`is_posix`], which only
/// ever fires on Windows.
pub fn path_searched(path: Option<&str>, windows: bool) -> String {
    let entries = split_path(path.unwrap_or_default(), windows);
    if entries.is_empty() {
        // Not pedantry: an empty PATH is itself the whole answer, and it is what
        // a task that builds a child's environment from scratch tends to hand it.
        return "vm ▸ the PATH it searched was empty".to_string();
    }
    let unusable = entries.iter().filter(|e| e.posix).count();
    let plural = if entries.len() == 1 {
        "entry"
    } else {
        "entries"
    };
    let mut out = match unusable {
        0 => format!("vm ▸ the PATH it searched ({} {plural}):", entries.len()),
        n => format!(
            "vm ▸ the PATH it searched ({} {plural}, {n} of them unusable):",
            entries.len()
        ),
    };
    for entry in &entries {
        if entry.posix {
            out.push_str(&format!("\n    ⚠ {}", elide(entry.text, ENTRY_WIDTH)));
            out.push_str(&match posix_paths(entry.text) {
                1 => "\n      ↑ POSIX form — no Win32 resolver can search it".to_string(),
                n => format!(
                    "\n      ↑ POSIX form, colon-joined: {n} paths, none of them searchable \
                     by a Win32 resolver"
                ),
            });
        } else {
            out.push_str(&format!("\n      {}", entry.text));
        }
    }
    out
}

/// The note for a PATH that reached a native Windows process still (partly) in
/// POSIX form — the failure behind #25, where three converted entries were
/// followed by the rest of the list as one colon-joined POSIX run. Win32 searches
/// the three and stops, so a `cargo` that is plainly installed comes back
/// not-found, and every obvious next move (reinstall it, run `where cargo`, echo
/// `%PATH%` from a *different* shell) looks fine.
///
/// Suggestive about the cause, certain about the fact: that those bytes cannot be
/// searched needs no theory about whose bug it is, while the trigger — a mise task
/// with `shell = "bash -c"`, whose Git Bash hands native grandchildren a
/// half-converted PATH — is the observed source, not a proven one. So it is named
/// as the usual suspect rather than as the verdict.
///
/// `None` on unix, always: a PATH there is POSIX by definition and nothing in it
/// classifies (see [`split_path`]).
pub fn half_posix_path_note(path: Option<&str>, windows: bool) -> Option<String> {
    let entries = split_path(path.unwrap_or_default(), windows);
    if !entries.iter().any(|e| e.posix) {
        return None;
    }
    Some(format!(
        "a POSIX PATH reached a native Windows process — nothing on Win32 can search those \
         entries, so the command was never going to be found there, however well it is \
         installed.\
         {INDENT}The usual source is a shell that converts only the head of the list on the \
         way out: a mise task with `shell = \"bash -c\"` hands its native grandchildren \
         exactly this.\
         {INDENT}Run the task without the bash shell, or pass a Windows PATH explicitly with \
         `-e PATH=…`."
    ))
}

/// One entry of a PATH, and whether any resolver can search it.
struct PathEntry<'a> {
    text: &'a str,
    posix: bool,
}

/// A PATH split the way the *target* platform splits it — `;` on Windows, `:` on
/// unix — with the unusable entries told apart. Empty entries are dropped: they
/// name no directory, and a blank line in the report would read as a bug.
///
/// The classification is Windows-only by construction. On unix a leading `/` is
/// simply what a path looks like, and the flag would fire on every entry of every
/// healthy PATH there is.
fn split_path(path: &str, windows: bool) -> Vec<PathEntry<'_>> {
    let sep = if windows { ';' } else { ':' };
    path.split(sep)
        .filter(|text| !text.is_empty())
        .map(|text| PathEntry {
            text,
            posix: windows && is_posix(text),
        })
        .collect()
}

/// Whether a Windows PATH entry is really a POSIX one. Two cheap tells, and
/// neither has to know how the entry got there:
///
/// - it starts with `/` — `C:\tools` and `\\host\share` do not;
/// - it holds a `:` *past* index 1 — a drive letter's colon sits exactly at 1, so
///   a later one is the POSIX separator, which means `;` never split this entry
///   and a whole colon-joined run of POSIX paths is hiding inside it.
fn is_posix(entry: &str) -> bool {
    entry.starts_with('/') || entry.match_indices(':').any(|(i, _)| i > 1)
}

/// How many POSIX paths are packed into one unsearchable entry — 1 for a lone
/// `/c/tools`, and the length of the run for a colon-joined tail.
fn posix_paths(entry: &str) -> usize {
    entry.split(':').filter(|p| !p.is_empty()).count()
}

/// `s` cut to `max` characters, marked `…` whenever anything was cut. Counts
/// characters, not bytes: a path with an umlaut in it must not get sliced through
/// a codepoint and panic on the way to reporting someone else's error.
fn elide(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

fn backticked(ops: &[String]) -> String {
    ops.iter()
        .map(|o| format!("`{o}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No file exists — isolates the exec-form rule from the filesystem one.
    fn no_files(_: &str) -> bool {
        false
    }

    fn notes(cmd: &[&str], is_file: impl Fn(&str) -> bool) -> Vec<String> {
        let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
        advisories("exec", "lin", &cmd, is_file)
    }

    /// The single note `cmd` produces, or a panic — most cases assert on its text.
    fn note(cmd: &[&str], is_file: impl Fn(&str) -> bool) -> String {
        let notes = notes(cmd, is_file);
        assert_eq!(notes.len(), 1, "expected exactly one note for {cmd:?}");
        notes.into_iter().next().unwrap()
    }

    fn silent(cmd: &[&str], is_file: impl Fn(&str) -> bool) {
        let notes = notes(cmd, is_file);
        assert!(notes.is_empty(), "{cmd:?} should be silent, got: {notes:?}");
    }

    // ── Advisory 1: exec form holding shell syntax ────────────────────────────

    #[test]
    fn a_lone_operator_in_exec_form_is_noted() {
        // Every operator, each in a realistic command. The whole point of the
        // note is that none of these *fail* — they run, wrongly, in silence.
        for cmd in [
            &["echo", "a", "&&", "echo", "b"][..],
            &["cargo", "build", "||", "true"][..],
            &["echo", "hi", "|", "wc"][..],
            &["sort", "f", ">", "out"][..],
            &["sort", "f", ">>", "out"][..],
            &["cat", "<", "in"][..],
            &["cat", "<<", "EOF"][..],
            &["sleep", "5", "&"][..],
            &["prog", "2>", "err"][..],
            &["prog", "2>>", "err"][..],
            &["prog", "2>&1"][..],
        ] {
            let note = note(cmd, no_files);
            assert!(note.contains("ONE argument"), "{cmd:?}: {note}");
        }
    }

    #[test]
    fn the_note_names_the_operator_the_program_and_the_fix() {
        let note = note(&["echo", "a", "&&", "echo", "b"], no_files);
        assert!(note.contains("`&&`"), "{note}");
        assert!(note.contains("`echo`"), "{note}");
        // The suggestion is the command the caller should have typed.
        assert!(note.contains("vm exec lin -- 'echo a && echo b'"), "{note}");
    }

    /// Both form notes end in a command line to paste, so they must name the
    /// verb the caller typed. `vm run` has no repo and no sync; being told to
    /// retry with `vm exec` would send them somewhere they cannot go.
    #[test]
    fn the_suggested_fix_names_the_verb_that_was_used() {
        let cmd: Vec<String> = ["echo", "a", "&&", "b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let note = advisories("run", "lin", &cmd, no_files).remove(0);
        assert!(note.contains("vm run lin -- 'echo a && b'"), "{note}");

        let script = vec!["my script.sh --flag".to_string()];
        let note = advisories("run", "lin", &script, files(&["my script.sh"])).remove(0);
        assert!(
            note.contains(r#"vm run lin -- '"my script.sh" --flag'"#),
            "{note}"
        );
    }

    #[test]
    fn several_operators_yield_one_note_listing_each_once() {
        let note = note(&["echo", "|", "x", "&&", "y", "|", "z"], no_files);
        assert!(note.contains("`|`, `&&`"), "{note}");
        assert_eq!(note.matches("`|`").count(), 1, "deduplicated: {note}");
    }

    #[test]
    fn an_operator_with_no_predecessor_still_fires() {
        // Degenerate, but the value-position check must not panic at index 0.
        assert!(note(&["&&", "x"], no_files).contains("`&&`"));
    }

    #[test]
    fn a_plain_command_is_silent() {
        silent(&["cargo", "test", "--workspace"], no_files);
        silent(&["cargo", "nextest", "run", "-p", "vm"], no_files);
        silent(&["echo", "hello"], no_files);
    }

    #[test]
    fn an_operator_in_a_flags_value_position_is_silent() {
        // `awk -F '|'` is a field separator, `tr -d '&'` a character set — the
        // literal operator is exactly what the caller meant.
        silent(&["awk", "-F", "|", "file"], no_files);
        silent(&["cut", "-d", "|", "-f1", "file"], no_files);
        silent(&["tr", "-d", "&"], no_files);
        silent(&["sort", "-t", "|", "-k1"], no_files);
    }

    #[test]
    fn a_multi_char_operator_after_a_flag_still_fires() {
        // The value-position escape hatch must not swallow the commonest real
        // case of all: no flag anywhere takes `&&` as its value.
        assert!(
            note(
                &["cargo", "build", "--release", "&&", "cargo", "test"],
                no_files
            )
            .contains("`&&`")
        );
        assert!(note(&["prog", "--verbose", "2>&1"], no_files).contains("`2>&1`"));
    }

    #[test]
    fn a_semicolon_is_never_noted() {
        // `find … -exec … {} ';'` passes it legitimately, in a non-value
        // position — indistinguishable from a shell `;`, so it is left alone.
        silent(&["find", ".", "-exec", "grep", "x", "{}", ";"], no_files);
        silent(&["echo", "a", ";", "echo", "b"], no_files);
    }

    #[test]
    fn an_operator_inside_a_larger_argument_is_silent() {
        // Not a lone operator: the caller quoted it into one word deliberately,
        // and it is a regex / literal, not syntax.
        silent(&["grep", "a|b", "file"], no_files);
        silent(&["echo", "a && b"], no_files);
        silent(&["echo", "&&&"], no_files);
        silent(&["echo", "-->"], no_files);
        silent(&["echo", "2>&11"], no_files);
        silent(&["echo", ""], no_files);
    }

    // ── Advisory 2: shell form whose script starts with a spaced filename ─────

    /// A filesystem holding exactly the given paths.
    fn files(paths: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p| paths.contains(&p)
    }

    #[test]
    fn a_script_starting_with_a_spaced_filename_is_noted() {
        let note = note(
            &["scripts/my script.sh --flag"],
            files(&["scripts/my script.sh"]),
        );
        assert!(note.contains("`scripts/my script.sh`"), "{note}");
        // The fix quotes the path and keeps the rest of the script intact.
        assert!(
            note.contains(r#"vm exec lin -- '"scripts/my script.sh" --flag'"#),
            "{note}"
        );
    }

    #[test]
    fn the_whole_script_being_the_file_is_noted_too() {
        assert!(note(&["my script.sh"], files(&["my script.sh"])).contains("`my script.sh`"));
    }

    #[test]
    fn a_windows_style_path_is_normalized_before_the_lookup() {
        let note = note(
            &[r"scripts\my script.sh --flag"],
            files(&["scripts/my script.sh"]),
        );
        // Quoted as the caller wrote it — the guest is what runs it.
        assert!(note.contains(r"`scripts\my script.sh`"), "{note}");
    }

    #[test]
    fn the_shortest_matching_prefix_wins() {
        // Both `a b` and `a b c` exist; the shell splits at the first, so that
        // is the one to quote.
        let note = note(&["a b c"], files(&["a b", "a b c"]));
        assert!(note.contains("`a b`"), "{note}");
    }

    #[test]
    fn a_script_that_names_no_file_is_silent() {
        // The overwhelmingly common shell-form case — must never say a word.
        silent(&["cd src && cargo test"], no_files);
        silent(&["cargo test --workspace"], no_files);
        silent(&["echo hi | tr a-z A-Z"], no_files);
        silent(&["exit 42"], no_files);
        silent(&["ls"], no_files);
    }

    #[test]
    fn a_spaceless_filename_is_silent() {
        // `run.sh --flag` word-splits into `run.sh` + `--flag`, which is exactly
        // what the caller wants. Only a space *inside the filename* is trouble.
        silent(&["./run.sh --flag"], files(&["./run.sh"]));
        silent(&["./run.sh"], files(&["./run.sh"]));
    }

    #[test]
    fn shell_form_never_reports_shell_syntax() {
        // A pipe in shell form is a working pipe — noting it would be a lie.
        silent(&["a | b"], no_files);
        silent(&["make && make test"], no_files);
    }

    #[test]
    fn a_file_named_later_in_the_script_is_silent() {
        // Only a *leading* spaced filename is word-split into a broken command
        // name; one appearing as an argument is the caller's own quoting problem
        // and not something the arity rule caused.
        silent(&["cat my file.txt"], files(&["my file.txt"]));
    }

    // ── Advisory 3: gitignored env files the sync left behind ─────────────────

    fn env_note(unsynced: &[&str]) -> String {
        let unsynced: Vec<String> = unsynced.iter().map(|s| s.to_string()).collect();
        unsynced_env_note(&unsynced).expect("a note")
    }

    #[test]
    fn nothing_unsynced_says_nothing() {
        // Every healthy repo without a gitignored .env — i.e. most of them — and
        // every failing command in one. The channel stays cheap.
        assert!(unsynced_env_note(&[]).is_none());
    }

    #[test]
    fn the_note_names_the_file_and_both_fixes() {
        let note = env_note(&[".env"]);
        assert!(note.contains("`.env` is gitignored"), "{note}");
        // The value fix first (nothing touches the guest's disk), the file fix
        // second — and the file fix is a flag the reader can paste as-is.
        assert!(note.contains("-e NAME=value"), "{note}");
        assert!(note.contains("--with-file .env"), "{note}");
    }

    #[test]
    fn several_files_read_as_a_list() {
        let note = env_note(&[".env", ".env.local"]);
        assert!(
            note.contains("`.env`, `.env.local` are gitignored"),
            "{note}"
        );
        assert!(note.contains("left them on the host"), "{note}");
        // The suggested flag names one of them; repeating it is the caller's call.
        assert!(note.contains("--with-file .env"), "{note}");
    }

    // ── Advisory 4: input wired into vm's own stdin ───────────────────────────

    #[test]
    fn ordinary_stdin_says_nothing() {
        // A terminal, /dev/null, an agent harness, CI — every run that did not
        // wire data into vm classifies as None and stays silent.
        assert!(stdin_note(None).is_none());
    }

    #[test]
    fn piped_stdin_draws_the_note_and_the_file_fix() {
        let note = stdin_note(Some(StdinSource::Piped)).expect("a note");
        assert!(note.contains("piped into vm"), "{note}");
        assert!(note.contains("does not travel to the guest"), "{note}");
        // The fix is the sync itself: put the data in a file, and it travels.
        assert!(note.contains("file inside the repo"), "{note}");
    }

    #[test]
    fn redirected_stdin_names_the_redirect() {
        let note = stdin_note(Some(StdinSource::Redirected)).expect("a note");
        assert!(note.contains("redirected into vm from a file"), "{note}");
    }

    // ── Advisory 5: the PATH behind a command-not-found ───────────────────────

    /// The PATH off the `windows-latest` runner in #25, trimmed: three converted
    /// Windows entries, and then the entire rest of the list still in POSIX form
    /// — which, holding no `;`, survives the split as one entry. `cargo` lives in
    /// that tail, and Win32 was never going to look there.
    const HALF_POSIX: &str = r"C:\Program Files\Git\mingw64\bin;C:\Program Files\Git\usr\bin;C:\Users\runneradmin\bin;/c/Program Files/Git/mingw64/bin:/c/Program Files/Git/usr/bin:/c/Users/runneradmin/.cargo/bin";

    /// Every assertion here injects `windows` rather than reading `cfg!(windows)`,
    /// so the Windows rules are tested on the machine the maintainer is sitting
    /// at. A `#[cfg(windows)]` on this section would re-create the exact blind
    /// spot that let #24 ship off a runner nobody was testing on.
    #[test]
    fn the_report_lists_the_entries_it_searched() {
        let report = path_searched(Some("/usr/local/bin:/usr/bin:/bin"), false);
        assert!(report.contains("(3 entries)"), "{report}");
        for dir in ["/usr/local/bin", "/usr/bin", "/bin"] {
            assert!(report.contains(dir), "{dir} missing from: {report}");
        }
    }

    #[test]
    fn a_unix_path_is_never_unusable() {
        // Every entry starts with `/`. On unix that is not a finding, it is a
        // PATH — the classifier must not fire on literally every healthy run.
        let path = "/usr/local/bin:/usr/bin:/bin";
        assert!(!path_searched(Some(path), false).contains('⚠'), "flagged");
        assert!(half_posix_path_note(Some(path), false).is_none());
    }

    #[test]
    fn a_healthy_windows_path_is_never_unusable() {
        let path = r"C:\Windows\system32;C:\Users\me\.cargo\bin;\\host\share\bin";
        let report = path_searched(Some(path), true);
        assert!(report.contains("(3 entries)"), "{report}");
        assert!(!report.contains('⚠'), "{report}");
        assert!(half_posix_path_note(Some(path), true).is_none());
    }

    #[test]
    fn a_drive_letters_colon_is_not_a_posix_separator() {
        // The whole rule rests on this: `C:\tools` carries a colon at index 1,
        // and only a colon *past* it means the entry is a colon-joined POSIX run.
        assert!(!is_posix(r"C:\tools"));
        assert!(!is_posix(r"C:/tools")); // mixed slashes, still a Win32 path
        assert!(is_posix("/c/tools"));
        assert!(is_posix("/c/a:/c/b"));
    }

    #[test]
    fn a_half_posix_path_is_flagged_counted_and_explained() {
        let report = path_searched(Some(HALF_POSIX), true);
        // The headline number is the finding: usable entries, then the rest.
        assert!(
            report.contains("(4 entries, 1 of them unusable)"),
            "{report}"
        );
        assert!(
            report.contains('⚠'),
            "the POSIX tail is not marked: {report}"
        );
        // Its three colon-joined paths are counted, not just shown.
        assert!(report.contains("colon-joined: 3 paths"), "{report}");
        // The usable entries still print — the reader has to see what *was*
        // searched to believe what was not.
        assert!(report.contains(r"C:\Users\runneradmin\bin"), "{report}");
    }

    #[test]
    fn the_note_names_the_trigger_and_both_fixes() {
        let note = half_posix_path_note(Some(HALF_POSIX), true).expect("a note");
        assert!(
            note.contains("POSIX PATH reached a native Windows process"),
            "{note}"
        );
        // The observed trigger, named as the usual source and not as a verdict:
        // vm cannot prove whose bug it is, only that the PATH is unsearchable.
        assert!(note.contains("The usual source"), "{note}");
        assert!(note.contains(r#"shell = "bash -c""#), "{note}");
        // Both ways out, in the order a reader can act on them.
        assert!(note.contains("without the bash shell"), "{note}");
        assert!(note.contains("-e PATH=…"), "{note}");
    }

    #[test]
    fn a_lone_posix_entry_is_flagged_without_the_colon_wording() {
        // One POSIX dir among Windows ones — unsearchable all the same, but there
        // is no colon-joined run to count, and claiming "1 paths" would be sloppy.
        let report = path_searched(Some(r"C:\Windows\system32;/usr/bin"), true);
        assert!(
            report.contains("(2 entries, 1 of them unusable)"),
            "{report}"
        );
        assert!(
            report.contains("POSIX form — no Win32 resolver"),
            "{report}"
        );
        assert!(!report.contains("colon-joined"), "{report}");
    }

    #[test]
    fn a_giant_posix_run_is_elided_but_its_paths_are_all_counted() {
        // The real one was kilobytes. The shape is the finding; the count is the
        // proof — and nothing is cut without the `…` and the number saying so.
        let tail = (0..60)
            .map(|i| format!("/c/tools/dir{i}/bin"))
            .collect::<Vec<_>>()
            .join(":");
        let report = path_searched(Some(&format!(r"C:\Windows\system32;{tail}")), true);
        assert!(report.contains('…'), "elided without saying so: {report}");
        assert!(report.contains("colon-joined: 60 paths"), "{report}");
        // The elision is of the *entry*, not of the report: no line runs away.
        for line in report.lines() {
            assert!(line.chars().count() < 100, "runaway line: {line}");
        }
    }

    #[test]
    fn an_empty_or_unset_path_says_exactly_that() {
        // The child was handed nothing to search, which is the entire diagnosis.
        for path in [None, Some(""), Some(";;")] {
            let report = path_searched(path, true);
            assert_eq!(report, "vm ▸ the PATH it searched was empty", "{path:?}");
        }
        assert!(half_posix_path_note(None, true).is_none());
    }

    #[test]
    fn elide_never_splits_a_codepoint() {
        // A path with an umlaut must not panic vm on its way to reporting someone
        // else's error. Cutting at a byte index would do exactly that.
        assert_eq!(elide("ööööö", 3), "ööö…");
        assert_eq!(elide("ööö", 3), "ööö");
        assert_eq!(elide("ab", 5), "ab");
    }

    // ── Cross-cutting ────────────────────────────────────────────────────────

    #[test]
    fn the_two_advisories_are_mutually_exclusive() {
        // Arity picks the form, so no command can ever draw both notes — a
        // single argument is a script, several are an argv, never both.
        let candidates: &[&[&str]] = &[
            &["echo", "a", "&&", "b"],
            &["a && b"],
            &["my file.txt"],
            &["cat", "my file.txt"],
            &["cargo", "test"],
        ];
        for cmd in candidates {
            assert!(
                notes(cmd, files(&["my file.txt"])).len() <= 1,
                "{cmd:?} drew more than one note"
            );
        }
    }
}
