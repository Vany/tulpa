#!/bin/sh
# Harness for scripts/public-check.sh: throwaway repositories, a throwaway word list of generic words, and the
# addresses and names in test/fixtures/public-check/ (flagged.txt must fail, allowed.txt must pass).
#
#   sh test/public-check/run.sh   (TEST_SHELL=bash runs the gate under bash; TEST_KEEP=1 keeps the repositories)
#
# (a) a word of the list in the tip's tree fails, in any case, listed as path:line (nested paths, spaces, binary
#     files and test/fixtures/ too); a pattern is an extended regular expression; the working copy is not read;
# (b) a word in a commit message, author or committer of the range fails, naming the commit and the field; a
#     commit outside the range does not; the tree checked is the range's tip;
# (c) every line of flagged.txt fails in the tree, in a message and as an added line, and an author address from
#     it fails; every line of allowed.txt passes in all three; flagged.txt passes under the top test/fixtures/,
#     not under another one;
# (d) setup errors exit 2: arguments, a missing or blank list, a pattern that does not compile or that matches
#     an empty line, a range that does not resolve, has two tips or is an option;
# (e) the list is never printed: no run's output holds the pattern that never matches;
# (f) a word or an address that a commit of the range adds fails even when a later commit removes it, listed as
#     the commit and path:line: the line counted in the new file, hunk by hunk; a path with a space or one that
#     git quotes; a line starting with ++; a binary file; a root commit; a merge's own lines only; whatever the
#     user's diff settings; a pattern's ^ at the line's start. A line the range removes or leaves does not
#     count; test/fixtures/ is exempt from the addresses, not from the list.
set -u
here=$(cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(cd -- "$here/../.." && pwd)
gate_script=$repo_root/scripts/public-check.sh
fixtures=$repo_root/test/fixtures/public-check
shell=${TEST_SHELL:-sh}
root=$(mktemp -d)
trap 'if [ -n "${TEST_KEEP:-}" ]; then printf "kept %s\n" "$root"; else rm -rf "$root"; fi' EXIT

# git without the user's own configuration (identity, signing, hooks), under a neutral identity.
HOME=$root/home
XDG_CONFIG_HOME=$HOME/.config
GIT_CONFIG_GLOBAL=$HOME/.gitconfig
GIT_CONFIG_NOSYSTEM=1
GIT_AUTHOR_NAME=Tester
GIT_AUTHOR_EMAIL=tester@example.invalid
GIT_COMMITTER_NAME=Tester
GIT_COMMITTER_EMAIL=tester@example.invalid
export HOME XDG_CONFIG_HOME GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE
mkdir -p "$HOME"
fails=0

# CRLF line ends and blank lines on purpose. The last pattern matches nothing here and must never be shown.
words=$root/words.txt
printf 'secretname\r\n\r\nhush-host\r\n#9[0-9]{3}\r\n  \r\nneverprinted-[0-9]+\r\n' >"$words"

check() {
	if eval "$2"; then
		printf 'ok   %s\n' "$1"
	else
		printf 'FAIL %s\n' "$1"
		fails=$((fails + 1))
	fi
}

# Runs the gate in the current repository: its output in $out, its exit code in $status; (e) on every run.
gate() {
	$shell "$gate_script" "$@" </dev/null >"$out" 2>&1
	status=$?
	if grep -q neverprinted "$out"; then
		printf 'FAIL (e) the list was printed by: public-check.sh %s\n' "$*"
		fails=$((fails + 1))
	fi
}

# A new repository with one clean commit that origin/main points at.
fresh() {
	dir=$root/$1
	out=$root/$1.out
	mkdir -p "$dir" && cd "$dir" || exit 2
	git -c init.defaultBranch=main init -q .
	printf 'clean\n' >README
	git add README
	git commit -q -m 'Start'
	git update-ref refs/remotes/origin/main HEAD
}

# Commits a file with the given content (one or more lines).
commit_file() {
	mkdir -p "$(dirname -- "$1")"
	printf '%s\n' "$2" >"$1"
	git add -- "$1"
	git commit -q -m 'Add a file'
}

# The fixture lines, without comments and blank lines.
lines_of() { grep -v -e '^#' -e '^[[:space:]]*$' "$fixtures/$1"; }

fresh a-clean
gate "$words"
check "(a) a clean repository passes" '[ "$status" -eq 0 ] && grep -q "^public-check: pass: .* 4 pattern(s)" "$out"'

fresh a-word
commit_file notes.txt "$(printf 'fine\na SecretName here')"
gate "$words"
check "(a) a word fails in any case, as path:line" '[ "$status" -eq 1 ] && grep -qx "notes.txt:2:a SecretName here" "$out"'
check "(a) the clean line is not listed" '! grep -q "^notes.txt:1:" "$out"'

fresh a-paths
commit_file "dir/sub dir/file.rs" "// HUSH-HOST"
gate "$words"
check "(a) a nested path with a space" '[ "$status" -eq 1 ] && grep -qx "dir/sub dir/file.rs:1:// HUSH-HOST" "$out"'

fresh a-regex
commit_file card.txt "see #91 and #9x12"
gate "$words"
check "(a) a pattern is a regular expression: #9[0-9]{3} misses #91" '[ "$status" -eq 0 ]'
commit_file card.txt "fixed in #9123"
gate "$words"
check "(a) a pattern is a regular expression: #9[0-9]{3} hits #9123" '[ "$status" -eq 1 ] && grep -qx "card.txt:1:fixed in #9123" "$out"'

fresh a-binary
printf 'x\000y secretname\n' >blob.bin
git add blob.bin && git commit -q -m 'Add a blob'
gate "$words"
check "(a) a binary file is named" '[ "$status" -eq 1 ] && grep -qx "Binary file blob.bin matches" "$out"'

fresh a-fixtures
commit_file test/fixtures/sample.txt "secretname"
gate "$words"
check "(a) test/fixtures/ is not exempt from the list" '[ "$status" -eq 1 ] && grep -q "^test/fixtures/sample.txt:1:" "$out"'

fresh a-worktree
printf 'secretname\n' >>README
printf 'secretname\n' >untracked.txt
gate "$words"
check "(a) the working copy is not read, only the commits" '[ "$status" -eq 0 ]'

fresh a-subdir
commit_file sub/leak.txt "secretname"
commit_file other/fine.txt "fine"
(cd other && $shell "$gate_script" "$words" </dev/null >"$out" 2>&1)
status=$?
check "(a) run from a subdirectory, the whole tree is read" '[ "$status" -eq 1 ] && grep -q "^sub/leak.txt:1:" "$out"'

fresh b-message
git commit -q --allow-empty -m 'Fix the SECRETNAME thing'
gate "$words"
abbrev=$(git rev-parse --short HEAD)
check "(b) a message's subject" '[ "$status" -eq 1 ] && grep -qx "commit $abbrev message line 1: Fix the SECRETNAME thing" "$out"'
git commit -q --allow-empty -m 'A subject' -m 'A body with hush-host in it'
gate "$words"
abbrev=$(git rev-parse --short HEAD)
check "(b) a message's body, by its line" '[ "$status" -eq 1 ] && grep -qx "commit $abbrev message line 3: A body with hush-host in it" "$out"'
GIT_AUTHOR_NAME=Secretname git commit -q --allow-empty -m 'Clean'
gate "$words" HEAD~1..HEAD
abbrev=$(git rev-parse --short HEAD)
check "(b) an author" '[ "$status" -eq 1 ] && grep -qx "commit $abbrev author: Secretname <tester@example.invalid>" "$out"'
GIT_COMMITTER_EMAIL=ops@hush-host.example git commit -q --allow-empty -m 'Clean'
gate "$words" HEAD~1..HEAD
abbrev=$(git rev-parse --short HEAD)
check "(b) a committer" '[ "$status" -eq 1 ] && grep -qx "commit $abbrev committer: Tester <ops@hush-host.example>" "$out"'
check "(b) only the commits of the range" '! grep -q "message line" "$out"'
check "(b) the summary counts the flagged commits" 'grep -q "^public-check: FAIL: 0 line(s) of the tree of [0-9a-f]*, 1 of 1 commit(s) of HEAD~1..HEAD$" "$out"'

fresh b-outside
git commit -q --allow-empty -m 'An early secretname'
git update-ref refs/remotes/origin/main HEAD
git commit -q --allow-empty -m 'Clean'
gate "$words"
check "(b) a commit before the range is not read" '[ "$status" -eq 0 ] && grep -q " 1 commit(s) of origin/main..HEAD, " "$out"'
gate "$words" HEAD~2..HEAD
check "(b) ... but is once the range holds it" '[ "$status" -eq 1 ] && grep -q "message line 1: An early secretname" "$out"'

fresh b-tip
git checkout -q -b side
commit_file leak.txt "secretname"
git checkout -q main
gate "$words"
check "(b) HEAD's tree by default" '[ "$status" -eq 0 ]'
gate "$words" main..side
check "(b) the tree of the range's tip, not HEAD's" '[ "$status" -eq 1 ] && grep -qx "leak.txt:1:secretname" "$out"'

fresh c-flagged
while IFS= read -r line; do
	commit_file notes.txt "$line"
	gate "$words" HEAD~1..HEAD
	check "(c) flagged in the tree: $line" '[ "$status" -eq 1 ] && grep -q "^notes.txt:1:" "$out"'
	abbrev=$(git rev-parse --short HEAD)
	check "(c) flagged as an added line: $line" 'grep -Fqx "commit $abbrev adds notes.txt:1: $line" "$out"'
	git commit -q --allow-empty -m "Note: $line"
	gate "$words" HEAD~1..HEAD
	check "(c) flagged in a message: $line" '[ "$status" -eq 1 ] && grep -q "message line 1: Note: " "$out"'
	git rm -q notes.txt && git commit -q -m 'Remove the note'
done <<EOF
$(lines_of flagged.txt)
EOF
local_address=$(lines_of flagged.txt | grep '@' | head -n 1)
GIT_AUTHOR_EMAIL=$local_address git commit -q --allow-empty -m 'Clean'
gate "$words" HEAD~1..HEAD
check "(c) flagged as an author's address: $local_address" '[ "$status" -eq 1 ] && grep -q "author: Tester <" "$out"'

fresh c-allowed
while IFS= read -r line; do
	commit_file notes.txt "$line"
	git commit -q --allow-empty -m "Note: $line"
	gate "$words" HEAD~2..HEAD
	check "(c) allowed in the tree, in a message and as an added line: $line" '[ "$status" -eq 0 ]'
done <<EOF
$(lines_of allowed.txt)
EOF

fresh c-fixtures
commit_file test/fixtures/addresses.txt "$(lines_of flagged.txt)"
gate "$words"
check "(c) test/fixtures/ may hold every flagged line" '[ "$status" -eq 0 ]'
commit_file crates/x/test/fixtures/addresses.txt "$(lines_of flagged.txt | head -n 1)"
gate "$words"
check "(c) ... only the top-level test/fixtures/" '[ "$status" -eq 1 ] && grep -q "^crates/x/test/fixtures/addresses.txt:1:" "$out"'

fresh d-errors
gate
check "(d) no arguments: exit 2" '[ "$status" -eq 2 ]'
gate "$words" HEAD~1..HEAD extra
check "(d) three arguments: exit 2" '[ "$status" -eq 2 ]'
gate "$root/no-such-list"
check "(d) a missing list: exit 2" '[ "$status" -eq 2 ]'
printf '\n  \r\n\n' >"$root/blank.txt"
gate "$root/blank.txt"
check "(d) a list of blank lines: exit 2" '[ "$status" -eq 2 ] && grep -q "has no patterns" "$out"'
printf 'secretname\n\nneverprinted-(\n' >"$root/bad.txt"
gate "$root/bad.txt"
check "(d) a pattern that does not compile: exit 2, by its line" '[ "$status" -eq 2 ] && grep -q "line 3 of the word list" "$out"'
printf 'secretname\n(neverprinted)?\n' >"$root/empty-match.txt"
gate "$root/empty-match.txt"
check "(d) a pattern that matches an empty line: exit 2, by its line" '[ "$status" -eq 2 ] && grep -q "line 2 of the word list matches an empty line" "$out"'
gate "$words" no-such-ref..HEAD
check "(d) a range that does not resolve: exit 2" '[ "$status" -eq 2 ]'
git checkout -q -b side
git commit -q --allow-empty -m 'Side'
git checkout -q main
git commit -q --allow-empty -m 'Main'
gate "$words" main...side
check "(d) a range with two tips: exit 2" '[ "$status" -eq 2 ] && grep -q "exactly one tip" "$out"'
gate "$words" --all
check "(d) an option as the range: exit 2" '[ "$status" -eq 2 ]'
git update-ref -d refs/remotes/origin/main
gate "$words"
check "(d) no origin/main for the default range: exit 2" '[ "$status" -eq 2 ]'

# The first flagged address. Most repositories below remove again what they add, so only an added line fails.
private=$(lines_of flagged.txt | head -n 1)

fresh f-history
commit_file notes.txt "$(printf 'fine\n%s' "$private")"
added=$(git rev-parse --short HEAD)
commit_file notes.txt "fine"
gate "$words"
check "(f) an address that one commit adds and the next removes fails, as the commit and path:line" \
	'[ "$status" -eq 1 ] && grep -Fqx "commit $added adds notes.txt:2: $private" "$out"'
check "(f) ... only that line, under the commit that adds it" \
	'[ "$(grep -c "^commit " "$out")" -eq 1 ] &&
	grep -q "^public-check: FAIL: 0 line(s) of the tree of [0-9a-f]*, 1 of 2 commit(s) of origin/main..HEAD$" "$out"'

fresh f-word
commit_file notes.txt "a SecretName here"
added=$(git rev-parse --short HEAD)
git rm -q notes.txt && git commit -q -m 'Remove the note'
gate "$words"
check "(f) a word of the list, added and removed, fails in any case" \
	'[ "$status" -eq 1 ] && grep -qx "commit $added adds notes.txt:1: a SecretName here" "$out"'

# The range replaces the address, keeps secretname and deletes hush-host.
fresh f-old
commit_file notes.txt "$(printf '%s\nsecretname\nhush-host' "$private")"
git update-ref refs/remotes/origin/main HEAD
commit_file notes.txt "$(printf 'clean\nsecretname')"
gate "$words"
check "(f) a line that the range removes or leaves is not listed as added" \
	'[ "$status" -eq 1 ] && grep -qx "notes.txt:2:secretname" "$out" && ! grep -q " adds " "$out"'

fresh f-fixtures
commit_file test/fixtures/addresses.txt "$(lines_of flagged.txt)"
commit_file test/fixtures/addresses.txt "clean"
gate "$words"
check "(f) test/fixtures/ may have held every flagged line" '[ "$status" -eq 0 ]'
commit_file test/fixtures/sample.txt "secretname"
added=$(git rev-parse --short HEAD)
git rm -q test/fixtures/sample.txt && git commit -q -m 'Remove the sample'
gate "$words"
check "(f) ... but no word of the list" \
	'[ "$status" -eq 1 ] && grep -qx "commit $added adds test/fixtures/sample.txt:1: secretname" "$out"'
git update-ref refs/remotes/origin/main HEAD
commit_file crates/x/test/fixtures/addresses.txt "$private"
added=$(git rev-parse --short HEAD)
git rm -q crates/x/test/fixtures/addresses.txt && git commit -q -m 'Remove the addresses'
gate "$words"
check "(f) ... and only the top-level test/fixtures/" \
	'[ "$status" -eq 1 ] && grep -Fqx "commit $added adds crates/x/test/fixtures/addresses.txt:1: $private" "$out"'

# Two lines inserted on top move the old lines 3 and 25 to 5 and 27; tail.txt had no newline at its end.
fresh f-lines
awk 'BEGIN { for (i = 1; i <= 30; i++) print "line " i }' >notes.txt
printf 'a\nb' >tail.txt
git add notes.txt tail.txt && git commit -q -m 'Add two files'
git update-ref refs/remotes/origin/main HEAD
awk -v p="$private" 'BEGIN {
	print "top"; print "top"
	for (i = 1; i <= 30; i++) print (i == 3 ? "secretname" : i == 25 ? p : "line " i)
}' >notes.txt
printf 'a\nb\n%s' "$private" >tail.txt
git add notes.txt tail.txt && git commit -q -m 'Change both files'
added=$(git rev-parse --short HEAD)
gate "$words"
check "(f) an added line is numbered in the new file, hunk by hunk" \
	'grep -qx "commit $added adds notes.txt:5: secretname" "$out" && grep -Fqx "commit $added adds notes.txt:27: $private" "$out"'
check "(f) ... past a missing newline at the end of the old file" \
	'grep -Fqx "commit $added adds tail.txt:3: $private" "$out"'

fresh f-plus
commit_file notes.txt "$(printf '++ %s\n++ b/secretname' "$private")"
added=$(git rev-parse --short HEAD)
git rm -q notes.txt && git commit -q -m 'Remove the note'
gate "$words"
check "(f) an added line that starts with ++ is a line, not the +++ header of a file" \
	'grep -Fqx "commit $added adds notes.txt:1: ++ $private" "$out" && grep -qx "commit $added adds notes.txt:2: ++ b/secretname" "$out"'

fresh f-path
commit_file "dir/sub dir/file.txt" "$private"
added=$(git rev-parse --short HEAD)
git rm -q "dir/sub dir/file.txt" && git commit -q -m 'Remove the file'
gate "$words"
check "(f) a path with a space" 'grep -Fqx "commit $added adds dir/sub dir/file.txt:1: $private" "$out"'

# git quotes a path with a byte above 127 (core.quotePath, on by default), as "caf\303\251.txt".
fresh f-quoted
e_acute=$(printf '\303\251')
commit_file "test/fixtures/caf$e_acute.txt" "$private"
gate "$words"
check "(f) a quoted path under test/fixtures/ is exempt too" '[ "$status" -eq 0 ]'
commit_file "caf$e_acute.txt" "secretname"
added=$(git rev-parse --short HEAD)
gate "$words"
quoted='"caf\303\251.txt"'
check "(f) a path that git quotes is shown quoted" 'grep -Fqx "commit $added adds $quoted:1: secretname" "$out"'

# Settings that would change a porcelain diff, and GIT_DIFF_OPTS, which adds context lines even to plumbing.
fresh f-config
git config diff.noprefix true
git config diff.mnemonicPrefix true
git config diff.suppressBlankEmpty true
git config diff.external false
git config color.ui always
printf 'a\n\nb\n' >notes.txt
git add notes.txt && git commit -q -m 'Add a note'
git update-ref refs/remotes/origin/main HEAD
printf 'a\n\n%s\nb\n' "$private" >notes.txt
git add notes.txt && git commit -q -m 'Add an address'
added=$(git rev-parse --short HEAD)
GIT_DIFF_OPTS=--unified=5
export GIT_DIFF_OPTS
gate "$words"
unset GIT_DIFF_OPTS
check "(f) the user's diff settings change nothing, and context lines are counted" \
	'[ "$status" -eq 1 ] && grep -Fqx "commit $added adds notes.txt:3: $private" "$out"'

fresh f-binary
printf 'x\000y\033[2J secretname\n' >blob.bin
git add blob.bin && git commit -q -m 'Add a blob'
added=$(git rev-parse --short HEAD)
git rm -q blob.bin && git commit -q -m 'Remove the blob'
gate "$words"
check "(f) a binary file is read as text, its control characters shown as ?" \
	'[ "$status" -eq 1 ] && grep -qx "commit $added adds blob.bin:1: x?y?\[2J secretname" "$out"'

dir=$root/f-root
out=$root/f-root.out
mkdir -p "$dir" && cd "$dir" || exit 2
git -c init.defaultBranch=main init -q .
commit_file notes.txt "secretname"
added=$(git rev-parse --short HEAD)
commit_file notes.txt "clean"
gate "$words" HEAD
check "(f) the root commit, when the range holds it" \
	'[ "$status" -eq 1 ] && grep -qx "commit $added adds notes.txt:1: secretname" "$out"'

# side changes line 2 to the address, main to TWO; the merge keeps side's line and adds one of its own.
fresh f-merge
commit_file notes.txt "$(printf 'one\ntwo\nthree')"
git update-ref refs/remotes/origin/main HEAD
git checkout -q -b side
commit_file notes.txt "$(printf 'one\n%s\nthree' "$private")"
side=$(git rev-parse --short HEAD)
git checkout -q main
commit_file notes.txt "$(printf 'one\nTWO\nthree')"
git merge -q --no-edit side >/dev/null 2>&1
printf 'one\n%s\nsecretname in a merge\nthree\n' "$private" >notes.txt
git add notes.txt && git commit -q -m 'Merge side'
merge=$(git rev-parse --short HEAD)
gate "$words"
check "(f) a merged branch's commit is read" 'grep -Fqx "commit $side adds notes.txt:2: $private" "$out"'
check "(f) a merge adds the lines that none of its parents has" \
	'grep -qx "commit $merge adds notes.txt:3: secretname in a merge" "$out"'
check "(f) ... not a line that it takes from a parent" '! grep -q "^commit $merge adds notes.txt:2:" "$out"'

printf '^anchored\n' >"$root/anchored.txt"
fresh f-anchor
commit_file notes.txt "anchored at the start"
added=$(git rev-parse --short HEAD)
commit_file notes.txt "clean"
gate "$root/anchored.txt"
check "(f) a pattern's ^ anchors at the start of the added line, not of the diff's line" \
	'[ "$status" -eq 1 ] && grep -qx "commit $added adds notes.txt:1: anchored at the start" "$out"'

if [ "$fails" -eq 0 ]; then
	printf 'public-check: all checks passed\n'
else
	printf 'public-check: %s check(s) failed\n' "$fails"
	exit 1
fi
