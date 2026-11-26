# Aurelis Companion

Aurelis Companion is a small helper that runs on a machine in your home and lets your Aurelis
television boxes share work. It does six things:

* **Shares AI answers.** When one box has asked an AI service for a column or a blurb, the others
  can reuse the answer instead of asking again.
* **Runs AI jobs for the boxes.** When a television has a question for an AI service that can wait
  for a free slot, it hands the job to the Companion, which keeps asking the free providers and
  hands the answer back. To do that it **uses your household's AI keys** to call those providers.
  The keys are held only inside sealed job files and are wiped when you unpair. Switch it off with
  the `ai_runner` option (`on` or `off`); the televisions then ask for themselves, as before.
  Pausing the Companion on its status page pauses the runner too, and the televisions are told so.
  Before paying for an answer it checks whether the house already holds it.
* **Relays Betfair for the boxes.** If your router sends the boxes' traffic through a VPN, Betfair
  refuses them. A paired box can then hand each of its Betfair calls to the Companion, which sends it
  to Betfair from its own connection and hands Betfair's answer back unchanged. It sends only to
  Betfair's two addresses (`identitysso.betfair.com` and `api.betfair.com`, over https) and refuses
  anything else. **Nothing is kept**: no login, password, app key, session or reply is written to
  disk, and its log lines name only the Betfair address, the call (`login`, `listMarketBook` …), the
  answer's status code and the time taken. The status page shows the last call the same way. It
  never places bets; Aurelis only reads. Switch it off with the `betfair_relay` option (`on` or
  `off`); the boxes then call Betfair themselves, as before. Pausing the Companion pauses it too.
  A wrong Betfair password may be tried once on each path (direct and via the Companion) per 30
  minutes: that is by design, so that a VPN refusal on one path never blocks a relayed login.
* **Relays live channels (optional).** A television can play a channel through the Companion, so
  boxes watching the same channel share one connection to your provider. This only helps when the
  Companion can reach your provider itself: if your televisions use a VPN the Companion is not on,
  the provider may refuse it. When it refuses three times in a row the Companion stops offering the
  relay for six hours and says why on its status page, and the televisions play directly. To stop
  it altogether, set the `stream_relay` option to `false`.
* **Downloads guides once.** It fetches your guide (XMLTV) files itself, on their own schedule, and
  hands the copy to each box, so the boxes do not each download the same large file.
* **Keeps a copy of the master television's household settings.** The master box puts its settings
  here so the other boxes can pick them up when the master is off or out of reach.
* **Keeps pictures, if you turn that on.** Posters and logos can be kept in one place for the whole
  household. This is **off by default**.

It never fetches video streams, and nothing is built into it: no key, login or address. Everything
it knows, a paired television told it.

## The Channels page

Open the Companion from Home Assistant's side panel and choose **Channels** to arrange the guide
for every television at once. The list on the left is the guide's groups, country by country; the
group itself is shown in the order the televisions draw it. Every change saves at once.

* **Your own categories.** **+ New category** adds a guide tab of its own, shown after the built-in
  ones on every television once it has a channel in it. Rename it with ✎ and delete it with ✕;
  deleting it sends its channels back to the groups Aurelis puts them in, without dial numbers.
* **Folders.** **+** beside any category adds a folder inside it (up to three deep), shown on the
  televisions as a tab nested under the category. Sports Extra's folders, including the ones the
  televisions make, can be renamed and deleted too; a deleted folder's channels move up a level.
* **Built-in categories** (Entertainment, News, Sport and the rest) carry a padlock: they cannot be
  renamed or deleted, because Aurelis files new channels into them by itself.
* To move a channel, open its ⋯ and choose the category or folder under **Group**.

A television on an older Aurelis build ignores categories and folders it does not know, and shows
those channels where it did before.

## What it holds, and your backups

Once paired, the Companion's data folder (`/data` in the add-on) holds things worth protecting:

* **The household secret** (`household.json`), which a television gave it when you paired.
* **The master television's household settings** (in `household/`). These are sealed, and the
  Companion never opens them, but they include **every key and login your household has entered**:
  AI, metadata, sports and odds keys, IPTV and media-server logins. The household secret in the same
  folder is what opens them, so anyone holding a copy of the whole folder holds your keys.
* **Your guide addresses** (`guide_sources.json`). An IPTV guide address often has your
  provider's username and password in it.

Home Assistant backups copy the add-on's data folder. The add-on leaves `household/` and
`guide_sources.json` out of its backups, but the household secret is still in them (so a restored
Companion stays paired). **Turn on encryption for your Home Assistant backups**, and keep backups of
a plain Docker install's data folder as carefully as you would a password list.

A Companion restored from a backup still has the secret but no copy of the master's settings until
the master next puts one there: within about five minutes of the master television running, or up
to 45 minutes if it is playing something at the time.

**Unpairing deletes all of it**: the secret, the master's settings, the guide addresses and every
store (see Unpairing below).

## What it needs

* A 64-bit machine (amd64 or arm64).
* At least **1 GB of RAM** in total.
* At least **2 GB free** in its data folder.

Below that it will not start, and it prints why. It looks after itself while it runs: if the machine
becomes busy it tells the boxes so, and they simply carry on without it.

## Pairing a television

One Companion serves one household. To pair:

1. Open the add-on's page in Home Assistant (**Open Web UI**). The pairing code is shown at the top
   of the page. On Docker, run `docker logs aurelis-companion`; the code is printed there.
2. On a television, open **Settings** and choose **Aurelis Companion**, then enter the code. The
   code has 8 characters, works once, and lasts 10 minutes; after five wrong tries a fresh one is made.
3. Once a household is paired the code is no longer shown.

The Companion announces itself on your network as **Aurelis Companion**, so a box normally finds it
without being told an address. This needs the machine to be on the same network as the boxes with
multicast allowed; the add-on uses host networking for exactly this. If your Docker setup cannot do
that, the Companion still works: enter its address (port 8765) on the television instead.

## Pictures

The picture store is **off by default**. To turn it on, switch **pictures** on in the add-on
options. Pictures are kept in the Companion's own data folder unless you set **picture_dir** to
a folder under `/share` or `/media`. On a machine whose storage is an SD card, pictures stay
off unless you set a picture folder somewhere that is not the SD card.

## Disk use

**disk_budget_mb** is how much space the Companion may use. `0` means automatic: 10% of the free
space when it first started, at most 20 GB. When it is full, the oldest things nobody has asked
for are removed first.

## Please read: two things to know about your network

**Do not put port 8765 behind a reverse proxy on the same machine.** The Companion shows the pairing
code and the Unpair button only to requests that come from Home Assistant or from the machine itself.
Behind a proxy on the same machine, every request would look local, and the pairing code and
Unpair button would show to your whole network.

**A paired television can ask the Companion to fetch web addresses for it, including ones on your
own network.** That is how guides and pictures are downloaded. It refuses anything that looks like a
video stream, but it does not refuse addresses inside your home. Pair only televisions you trust.

## Unpairing

On the add-on's page choose **Unpair**. A paired television can also ask to unpair. Unpairing **deletes everything the Companion held for the household**: the guides and their
addresses, the pictures, the shared answers, any AI jobs and the keys sealed inside them, the master's sealed settings, the secret, the televisions' playback states and
notifications, the camera names, any live-channel relays and their timeshift buffers, and the record of providers that refused it. It then shows a new pairing code.
It cannot be undone. Your settings on the add-on's page (the master switches and the options) are yours, not the household's, and stay as they are.

## Running it on plain Docker

```
docker compose -f companion/compose.yaml up -d --build     # from the repository root
docker logs aurelis-companion                              # the pairing code is printed here
```

The compose file runs it as an unprivileged user (`user: "10001:10001"`) on host networking. The image
itself runs as root because the Home Assistant add-on's data folder belongs to root and the add-on
cannot pick another user; with `docker run`, add `--user 10001:10001` yourself. Settings can come
from the environment instead of the add-on options: `AURELIS_PORT`, `AURELIS_PICTURES` (`on` or
`off`), `AURELIS_PICTURE_DIR`, `AURELIS_DISK_BUDGET_MB`, `AURELIS_AI_RUNNER` and `AURELIS_BETFAIR_RELAY` (each `on` or `off`, default `on`).
