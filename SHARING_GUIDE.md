# Sharing a world

> One world, one host at a time, everybody pulls.

This fork lets a self-hosted `hoard-server` share a save between accounts.
You make a group, invite the people you play with, and share a world into
it. Every member sees it in their own Library, pulls it before they play, and
one of them at a time hosts it: the host's session uploads, everyone else's
reads. When the host stops, the next person can take the seat.

## What sharing is, and is not

- **One world.** A share is one save. For a game that keeps several worlds in
  one folder (Valheim), it is one world out of that folder, not the folder.
- **One host at a time.** A shared world carries a *hosting lease*. The
  machine that holds it is the only one that can push. Two people cannot play
  the same world at once and both keep their changes; the second one's changes
  end up in a side copy, not on the server.
- **Everybody pulls.** Members' machines keep the shared world at the
  server's latest version while the game is closed. Nothing is restored while
  the game is running.
- **Not a game server.** Hoard moves save files; it does not host the game
  session. Whoever hosts the world in Hoard is whoever runs the game with that
  world, and the others join their game the way they always did.
- **Not co-op for saves that are not worlds.** Characters, profiles and
  settings stay on each machine. Sharing a game with no world layout Hoard
  knows shares its whole folder, which is rarely what you want for anything
  but a dedicated-server style save.

## Before you start

- **A self-hosted server on this fork's release.** The group, share and lease
  routes are in this fork's server, not upstream's and not Hoard Cloud. See the
  [self-hosting guide](SELF-HOST_GUIDE.md), and the note on
  [shared storage and quota](SELF-HOST_GUIDE.md#shared-worlds-and-the-owners-quota)
  there.
- **An account per member.** Every person needs their own user on that server
  (the web panel creates them, see the self-hosting guide) and a device token
  for each machine they play on.
- **This fork's desktop build, or its CLI, on every machine.** An upstream
  client shows none of it.
- **Steam Cloud off for Valheim, on every member's machine.** Steam's cloud
  sync and Hoard both want to be the one that decides which copy of
  `worlds_local/<world>.db` is newest, and Steam will overwrite the world Hoard
  just pulled. In Steam: Valheim → Properties → General → uncheck *Keep games
  saves in the Steam Cloud*. The share dialog reminds you, because it bites
  every time.

## Create a group and invite

A group is one owner plus members. The owner cannot leave it, and the
owner's account is the one whose quota holds every world shared into the
group (`HRD-D-0001`). Invites are one-time tokens: shown once, valid for a set
time, and whoever redeems one becomes a member.

**Desktop.** Open *Groups* in the sidebar.

1. *New group* → type a *Group name* → *Create*.
2. On the group, *Invite link*. The token is shown once; *Copy* it and send it
   to the person. It expires in 7 days.
3. They open *Groups* on their side, paste it under *Have an invite link?* and
   press *Join*.

The group card shows who owns it, the members, and the worlds shared into it.
The owner can *Remove* a member; a member can *Leave*.

**CLI.**

```sh
hoard group create "The Thursday crew"
hoard group list
hoard group invite "The Thursday crew"              # --expires 7d by default; 12h, 30m, 3600s
hoard group join <TOKEN>                            # on the member's machine
hoard group leave "The Thursday crew"               # a member; the owner cannot
```

`<group>` is the group's id or its exact name. When two groups carry the same
name, the command refuses and lists the ids, so use the id. An invite's
`--expires` needs a unit: a bare number is refused because `7` could mean days
or seconds.

## Share a world

Sharing is a move, not a copy: every version of the save moves from your
namespace into the group's, and its storage moves onto the group owner's
quota. Only the save's owner can share or unshare it, and it has to be tracked
on the machine you share from, because the list of worlds comes from its
folder.

**What travels.** For Valheim, the share names the world's files and nothing
else:

```
worlds_local/<World>.db
worlds_local/<World>.fwl
worlds_local/<World>.db.old
worlds_local/<World>.fwl.old
worlds_local/<World>_backup_*
```

Nothing under `characters_local/` ever travels: a character is the player's,
not the world's. Every member keeps their own. For any other game the whole
save folder is shared, because Hoard has no template that says which files
are the world.

**Desktop.** In the *Library*, open the save's menu and pick *Share*. The
*Share a world* dialog asks for the *Group*, and for a game with worlds the
*World*; *What travels* lists the files above. Confirm with *Share <World>*.
Members see it in their Library right away.

**CLI.**

```sh
hoard saves                                          # find the save id
hoard share <SAVE_ID> --group "The Thursday crew" --world Midgard
hoard share <SAVE_ID> --group "The Thursday crew"    # a game with no world layout: whole folder
```

Leave `--world` off for Valheim and the command refuses and lists the worlds
it found under the folder. `<SAVE_ID>` is the UUID from `hoard saves`; there is
no `game/label` form yet.

The share is refused while the save is already shared (unshare first to move
it to another group), and unsharing is refused while somebody holds its lease.

## Join and adopt on another machine

Once a world is shared, it appears in every member's *Library* marked *Shared
with you · <label> · <group> · owned by <owner>*, with no folder on their
machine yet. Adopting binds it to that machine's save folder for the game,
the same flow as any save that only exists on the server:

1. On the shared row, *Adopt into a folder*.
2. The dialog lists what detection found on this machine under *Detected on
   this machine*; one click links the save to that folder and it starts
   syncing here. If nothing is detected, *Scan this machine* or choose the
   folder yourself.

From then on the world is pulled to that folder whenever the game is closed
and the server has a newer version. Your own characters in the same folder
are untouched, because they are not in the share's file list.

Adopting is a desktop step today: the CLI lists the shared row in `hoard saves`
and `hoard restore <SAVE_ID> --to <folder>` brings the files down, but there is
no CLI verb that binds a shared save to a folder for ongoing sync.

## Playing

### The prompt

When a game with shared worlds starts, Hoard asks once: *<Game> started. Which
world?* The prompt appears in the in-game HUD (Alt+H) and in the app, and
offers, per world:

- **Host** — take the lease. Your session uploads. Refused if somebody else is
  already hosting; then the option reads *<name> is hosting it*.
- **View** — pull, never push. See [What View means](#what-view-means).
- **Not playing** — no role this session. The clock stops and Hoard does not
  ask again for this launch.

### The 60 s clock

If the game has exactly one shared world and nobody holds its lease, the
prompt shows *Hosting <World> in <n> s unless you choose*. After 60 seconds
with no answer, Hoard hosts on its own and tells you it was the engine's
choice (`HRD-D-0002`). With two shared worlds, or a lease that is not free,
there is no clock: nothing happens until you answer.

Evidence beats a prompt: if the game writes to the world before you have
answered and the lease is free, Hoard takes the lease then rather than wait
the clock out. You are hosting whatever you would have said.

### What View means

A viewer's machine holds the world at the server's version and never pushes.
If the game saves anyway, the second write gets a notice, *Viewing <World>,
and writing*, with a *Host it* button: the game is saving into a copy that is
never pushed, and hosting is the way to keep it. If
you do nothing, the session's writes go to a side copy when the game closes
and the folder goes back to the shared version.

### The lease, and who pays

The host's machine renews its lease every 30 seconds, and the server lets a
lease expire 5 minutes after the last renewal (`HRD-D-0003`). A machine that
loses power or its network stops holding the world on its own; nobody has to
notice it left. While you hold a lease the Library row reads *hosting ·
<group>* and members see *hosted by <name> · <elapsed>*.

When the game closes, the host's final upload finishes and the lease is
released. `hoard world lease <SAVE_ID>` says who holds it right now.

Every byte of a shared world sits in the group's storage, against the group
owner's quota, whoever pushed it. A self-hosted server has no quota unless the
admin set one, so on most servers this is bookkeeping; on one with per-user
limits, the owner is the account that needs the room.

**CLI.** The same four answers, and the lease:

```sh
hoard world claim <SAVE_ID>            # host (the default)
hoard world claim <SAVE_ID> --view     # view only: your writes stay on this machine
hoard world dismiss <SAVE_ID>          # not playing this session
hoard world release <SAVE_ID>          # give the lease back
hoard world lease <SAVE_ID>            # who hosts it right now
```

The outcome of a claim shows in `hoard sync logs` and `hoard world lease`.
`hoard saves` carries a *hosted here* / *hosted by <name>* column for shared
rows while a lease is live.

## When things go wrong

### Hosted by someone else

If you start the game while another member holds the lease, the app says
*<holder> is hosting <World>* and your changes stay on this machine. Nothing
you do in that session reaches the server. When the game closes, the
session's files go to a side copy and the folder is pulled back to the shared
version, so the next launch is on what the host left.

If you meant to be the host, ask them to *Release the world* (their Library
row's menu) or close their game, then claim it.

### Lease lost

*You lost the lease on <World>* means the server no longer counts your
machine as the host: another member took it over, or it expired because the
server was out of reach for over five minutes. Nothing pushes from here until
you host it again. Your session keeps playing; when the game closes, what it
wrote goes to a side copy if the lease is now somebody else's. Claim again
(*Host*, or `hoard world claim`) once the server is back and nobody else has
it.

### Side copies, and where they are

A side copy holds the world files from a session that could not push: a
viewer's writes, or a host's writes under somebody else's lease. Only the
share's own files move there, nothing else in the folder. They land next to
the restore conflict copies, under the sync service's state folder:

```
~/.local/share/hoard/conflicts/<save_id>/<timestamp>/     # Linux
```

On Windows and macOS it is the same `conflicts/` folder under the state
directory the [client-side table](SELF-HOST_GUIDE.md#the-client-side-where-things-live)
maps to. The notice about the side copy has an *Open side copy* button that
opens it. The same retention sweep as restore conflicts applies, 14 days by
default, so copy out anything you want to keep. To bring a side copy back
into play, host the world and copy the files over the ones in
`worlds_local/` while the game is closed.

### Take over, and force

*Take over* (the Library row's menu, or `hoard world force <SAVE_ID>`) takes
a live lease off its holder and makes you the host. It works only while the
holder has pushed nothing under that lease: a session that has already
uploaded cannot be taken, and the server refuses with *the host has pushed
under this lease; it ends when they release it*. The menu shows *Take over
(<name> pushed)* when that is the case, so you know before you try. Its use
is a machine that grabbed the lease and then went quiet, or a host who
launched by mistake; for anything else, ask them to release it.

Hosting also needs your copy to be current: a claim is refused with *the save
moved past your version: pull before hosting* if the server has a newer
version than your machine has pulled. Close the game, let the pull happen,
claim again.

### Unshare and leave

*Unshare* (the save's menu, or `hoard unshare <SAVE_ID>`) takes the world back
into your own namespace. Members stop seeing it; what they already pulled
stays on their machines. It is refused while somebody is hosting, so wait for
the release. Deleting a group needs every save unshared first; the server
refuses otherwise.

A member who leaves, or is removed, stops seeing the group's worlds. Anything
they adopted stays on their machine as files, but it no longer syncs. A save
they had shared into the group goes back to them.

## Limits

- **Valheim is the only game with a world template.** Any other game shares
  its whole save folder, and its members' own profiles in that folder travel
  with it. More templates are a matter of naming the files; ask, or send a
  pull request.
- **Self-hosted only.** Sharing is not supported on Hoard Cloud. A client
  signed into both uses it only against the self-hosted server.
- **Cloudflare's 100 MB body cap.** A proxied hostname on Cloudflare's free
  plan caps each request body at 100 MB. Uploads are one file per request, so
  it bites when a single world file passes 100 MB, which a long-running
  Valheim `.db` does. Use a tunnel to a hostname that is not proxied, or the
  LAN/VPN address; see
  [Behind a reverse proxy](SELF-HOST_GUIDE.md#behind-a-reverse-proxy).
- **No merging.** Two people's changes to one world are never combined. The
  lease exists so that the question does not come up; a side copy is what
  you get when it does.
