#!/bin/sh
# The gate before a push to the public repository: private words, private addresses and home paths.
#
# Usage: scripts/public-check.sh <word-list-file> [<rev-range>]
#
#   <word-list-file>  kept outside this repository. One extended regular expression per line, matched
#                     case-insensitively anywhere in a line; blank lines are skipped. A pattern matches inside
#                     longer words too, so anchor a short one, as in `(^|[^a-z])name`, and escape a literal dot.
#                     Case folding beyond ASCII depends on the locale: spell such variants out, as in `[Xx]`.
#   <rev-range>       what the push sends (default origin/main..HEAD). The tree checked is its tip's, not the
#                     working copy, so commit first.
#
# Exits 1 and lists each match when
#   (a) a line of the tip's tree, or
#   (b) a commit of the range: its message, author or committer, or a line it adds, even one that a later
#       commit removes (a merge adds only the lines that none of its parents has; binary files are read as text)
#   matches a pattern of the list; or when one of them holds
#   (c) a private IPv4 address (RFC 1918: 10/8, 172.16/12, 192.168/16; also the shared 100.64/10), a *.local
#       host name, or a home path /Users/<lowercase name>. Files under test/fixtures/ are exempt from (c), not
#       from the list. Never flagged: documentation addresses (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24)
#       and *.example names, DNS-SD service types (_ssh._tcp.local.), localhost.local, dotted file names
#       (.env.local) and /Users/Shared.
# A match is listed as path:line:text, as "commit <sha> author|committer|message line <n>: <text>", or as
# "commit <sha> adds <path>:<line>: <text>" with control characters shown as ?. It prints the matches only,
# never the list.
# Exit 0: clean. Exit 2: a usage or setup error (the list unreadable or empty, a pattern that does not compile
# or that matches an empty line, a range that does not resolve to exactly one tip).
set -u

me=public-check
die() {
	printf '%s: %s\n' "$me" "$*" >&2
	exit 2
}

[ $# -eq 1 ] || [ $# -eq 2 ] || die "usage: scripts/public-check.sh <word-list-file> [<rev-range>]"
list=$1
range=${2:-origin/main..HEAD}
case $range in -*) die "a range cannot start with -: $range" ;; esac
[ -f "$list" ] && [ -r "$list" ] || die "cannot read the word list $list"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/public-check.XXXXXX") || die "cannot make a temporary directory"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 2' HUP INT TERM

# The list without CRs (a CRLF file) and without blank lines (a blank pattern matches every line). Each
# pattern is tried alone, so that a bad one is named by its line number, never shown.
tr -d '\r' <"$list" >"$tmp/list" || die "cannot read the word list $list"
grep -v '^[[:space:]]*$' "$tmp/list" >"$tmp/words"
patterns=$(grep -c '' "$tmp/words")
[ "$patterns" -gt 0 ] || die "the word list $list has no patterns"
n=0
while IFS= read -r pattern || [ -n "$pattern" ]; do
	n=$((n + 1))
	case $pattern in *[![:space:]]*) ;; *) continue ;; esac
	printf 'x\n' | grep -E -i -e "$pattern" >/dev/null 2>&1
	[ $? -le 1 ] || die "line $n of the word list is not an extended regular expression"
	if printf '\n' | grep -E -i -q -e "$pattern" 2>/dev/null; then
		die "line $n of the word list matches an empty line, so it would match every line"
	fi
done <"$tmp/list"

git rev-parse --git-dir >/dev/null 2>&1 || die "not in a git repository"
# git grep on a tree sees only the current directory's part of it.
if [ "$(git rev-parse --is-inside-work-tree 2>/dev/null)" = true ]; then
	cd "$(git rev-parse --show-toplevel)" || die "cannot enter the top of the work tree"
fi
git rev-list --reverse "$range" >"$tmp/commits" 2>/dev/null || die "not a revision range here: $range"
# rev-parse prints the tips and the excluded (^) ends; a name it cannot resolve, it skips silently.
tips=$(git rev-parse --revs-only "$range" 2>/dev/null | grep -v '^\^')
[ "$(printf '%s' "$tips" | grep -c '')" -eq 1 ] || die "the range must have exactly one tip: $range"
tip=$(git rev-parse --verify -q "$tips^{commit}") || die "the tip of $range is not a commit"
short=$(git rev-parse --short "$tip")
commits=$(grep -c '' "$tmp/commits")

octet='(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])'
before='(^|[^0-9.])'
after='([^0-9]|$)'
re_10=$before'10\.'$octet'\.'$octet'\.'$octet$after
re_172=$before'172\.(1[6-9]|2[0-9]|3[01])\.'$octet'\.'$octet$after
re_192=$before'192\.168\.'$octet'\.'$octet$after
re_100=$before'100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.'$octet'\.'$octet$after
re_home='/Users/[a-z]'
# A name ending in .local, starting where a host name can start: not after a letter, digit, dot, _ or -.
# So neither a DNS-SD service type (_ssh._tcp.local.) nor a dotted file name (.env.local) is one.
re_local='(^|[^A-Za-z0-9._-])[A-Za-z0-9][A-Za-z0-9-]*(\.[A-Za-z0-9][A-Za-z0-9-]*)*\.local([^A-Za-z0-9_-]|$)'
# The same on a lowercased line padded with spaces, for awk.
re_local_padded='[^a-z0-9._-][a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)*\.local[^a-z0-9_-]'

# Keeps a .local line only if a .local name other than localhost.local is on it ("Binary file" lines stay).
drop_localhost() {
	RE=$re_local_padded awk '
		/^Binary file / { print; next }
		{
			probe = " " tolower($0) " "
			while (match(probe, /[^a-z0-9._-]localhost\.local[^a-z0-9_-]/))
				probe = substr(probe, 1, RSTART) substr(probe, RSTART + RLENGTH - 1)
			if (probe ~ ENVIRON["RE"])
				print
		}'
}

# Appends to $3, as n:text, the lines of $1 that match a pattern of the list and the lines of $2 that break an
# address rule; $2 is $1, or $1 with the lines exempt from those rules left empty.
scan() {
	grep -a -n -i -E -f "$tmp/words" "$1" >>"$3" 2>/dev/null
	[ $? -le 1 ] || die "grep cannot use the word list"
	grep -a -n -E -e "$re_10" -e "$re_172" -e "$re_192" -e "$re_100" -e "$re_home" "$2" >>"$3"
	[ $? -le 1 ] || die "grep failed on commit $sha"
	grep -a -n -i -E -e "$re_local" "$2" >"$tmp/scan-local"
	[ $? -le 1 ] || die "grep failed on commit $sha"
	drop_localhost <"$tmp/scan-local" >>"$3"
}

# Reads a commit's patch (diff-tree -U0, --cc for a merge) and writes the lines the commit adds, one for one:
# their text to $tmp/added, the same with the lines under test/fixtures/ left empty to $tmp/added.addr, and
# their path:line to $tmp/added.at. It counts a hunk's lines from its header, so an added line that starts
# with ++ is not taken for a file's +++ header.
added_lines() {
	ADDED=$tmp/added LC_ALL=C awk '
		BEGIN { text = ENVIRON["ADDED"]; addr = text ".addr"; at = text ".at" }
		left > 0 {
			# A line of the hunk: one column per parent, then the text. A - in a column: that parent has
			# the line and the commit does not. + in every column: the commit adds it.
			if (substr($0, 1, 1) == "\\")
				next
			p = substr($0, 1, width)
			if (index(p, "-"))
				next
			left--
			n++
			if (p == plus) {
				print substr($0, width + 1) >text
				print (exempt ? "" : substr($0, width + 1)) >addr
				print path ":" n >at
			}
			next
		}
		/^diff / { head = 1; path = ""; exempt = 0; next }
		head && /^\+\+\+ / {
			# b/path, or "b/path" for an unusual one, and a tab after a path with a space.
			path = substr($0, 5)
			sub(/\t$/, "", path)
			if (substr(path, 1, 2) == "b/")
				path = substr(path, 3)
			else if (substr(path, 1, 3) == "\"b/")
				path = "\"" substr(path, 4)
			exempt = path ~ /^"?test\/fixtures\//
			next
		}
		/^@@/ {
			# @@ -a,b +c,d @@, with one more @ and one more -a,b for each further parent of a merge.
			# +c,d: the first line of the commit that the hunk holds, and how many it holds (1 if no ,d).
			head = 0
			split($0, f, " ")
			width = length(f[1]) - 1
			plus = ""
			for (i = 0; i < width; i++)
				plus = plus "+"
			r = substr(f[width + 2], 2)
			i = index(r, ",")
			left = i ? substr(r, i + 1) + 0 : 1
			n = (i ? substr(r, 1, i - 1) : r) - 1
			next
		}'
}

# (a) and (c) on the tip's tree. A user's grep.column setting would add a field to every line.
ggrep() { git -c grep.column=false grep --no-color -n "$@"; }
: >"$tmp/tree.raw"
ggrep -i -E -f "$tmp/words" "$tip" -- . >>"$tmp/tree.raw" 2>/dev/null
[ $? -le 1 ] || die "git grep cannot use the word list (a pattern its regex library rejects?)"
ggrep -E -e "$re_10" -e "$re_172" -e "$re_192" -e "$re_100" -e "$re_home" "$tip" -- . ':(exclude)test/fixtures/' >>"$tmp/tree.raw"
[ $? -le 1 ] || die "git grep failed on the tree of $short"
ggrep -i -E -e "$re_local" "$tip" -- . ':(exclude)test/fixtures/' >"$tmp/local.raw"
[ $? -le 1 ] || die "git grep failed on the tree of $short"
drop_localhost <"$tmp/local.raw" >>"$tmp/tree.raw"
sed -e "s|^$tip:||" -e "s|^Binary file $tip:|Binary file |" "$tmp/tree.raw" |
	LC_ALL=C sort -t: -k1,1 -k2,2n -u >"$tmp/tree.hits"

# (b) and (c) on each commit of the range: line 1 its author, line 2 its committer, then its message. -a: a
# message that is not valid text in the locale would otherwise get only a "binary file matches" note.
# Then the lines it adds, from a plumbing patch with its options spelled out, so that no diff setting of the
# user's changes it or runs a command; --text reads a binary file as lines, a NUL in one becoming \001 for awk.
: >"$tmp/commit.hits"
flagged=0
while IFS= read -r sha; do
	git log -1 --no-show-signature --no-color --format='%an <%ae>%n%cn <%ce>%n%B' "$sha" >"$tmp/commit" ||
		die "cannot read commit $sha"
	: >"$tmp/commit.raw"
	scan "$tmp/commit" "$tmp/commit" "$tmp/commit.raw"
	git diff-tree -r --root --cc -U0 --text --no-ext-diff --no-textconv --no-color --src-prefix=a/ \
		--dst-prefix=b/ --no-commit-id "$sha" >"$tmp/patch" ||
		die "cannot read the changes of commit $sha"
	: >"$tmp/added"
	: >"$tmp/added.addr"
	: >"$tmp/added.at"
	LC_ALL=C tr '\000' '\001' <"$tmp/patch" | added_lines || die "cannot read the changes of commit $sha"
	: >"$tmp/added.raw"
	[ ! -s "$tmp/added.at" ] || scan "$tmp/added" "$tmp/added.addr" "$tmp/added.raw"
	[ -s "$tmp/commit.raw" ] || [ -s "$tmp/added.raw" ] || continue
	flagged=$((flagged + 1))
	abbrev=$(git rev-parse --short "$sha")
	LC_ALL=C sort -t: -k1,1n -u "$tmp/commit.raw" >"$tmp/commit.sorted"
	while IFS= read -r hit; do
		line=${hit%%:*}
		text=${hit#*:}
		case $line in
		1) printf 'commit %s author: %s\n' "$abbrev" "$text" ;;
		2) printf 'commit %s committer: %s\n' "$abbrev" "$text" ;;
		*) printf 'commit %s message line %s: %s\n' "$abbrev" "$((line - 2))" "$text" ;;
		esac
	done <"$tmp/commit.sorted" >>"$tmp/commit.hits"
	LC_ALL=C sort -t: -k1,1n -u "$tmp/added.raw" |
		AT=$tmp/added.at ABBREV=$abbrev LC_ALL=C awk '
			BEGIN { at = ENVIRON["AT"]; while ((getline line <at) > 0) where[++n] = line }
			{
				i = index($0, ":")
				print "commit " ENVIRON["ABBREV"] " adds " where[substr($0, 1, i - 1)] ": " substr($0, i + 1)
			}' |
		LC_ALL=C tr '\001-\010\013-\037\177' '[?*]' >>"$tmp/commit.hits"
done <"$tmp/commits"

status=0
if [ -s "$tmp/tree.hits" ]; then
	printf '%s: the tree of %s:\n' "$me" "$short"
	cat "$tmp/tree.hits"
	status=1
fi
if [ -s "$tmp/commit.hits" ]; then
	printf '%s: the commits of %s:\n' "$me" "$range"
	cat "$tmp/commit.hits"
	status=1
fi
if [ "$status" -eq 0 ]; then
	printf '%s: pass: the tree of %s and %s commit(s) of %s, %s pattern(s) and the address rules\n' \
		"$me" "$short" "$commits" "$range" "$patterns"
else
	printf '%s: FAIL: %s line(s) of the tree of %s, %s of %s commit(s) of %s\n' \
		"$me" "$(grep -c '' "$tmp/tree.hits")" "$short" "$flagged" "$commits" "$range"
fi
exit "$status"
