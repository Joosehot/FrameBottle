# Frame Bottle

A movie as a grid of frames, in one `.fbott` file. Every frame of the movie
is kept, and unpacks to exactly the bytes it was chopped into. A prototype.

```
fbottle pack <movie> [-o FILE.fbott] [--for size|play] [--explain]
fbottle verify <bottle> <movie>          every node against the movie's own frames
fbottle play <bottle> [--smooth N] [--bench]   N times the movie's frame rate (2); --bench: no window, timed
fbottle test <movie> [--smooth N] [--for size|play]   pack, verify, play twice without a window, report
fbottle info <bottle>
fbottle forget <bottle>                  empty the bottle's window in Nimbus
fbottle smoother <bottle>                how near a guessed frame comes to a real one
```

Build with `cargo build --release`. Frame Bottle runs no outside program
and does not use ffmpeg: Windows' Media Foundation opens the movie, and the
player has its own window. It needs Strike's sources next to this folder
(`../Strike`),
the particle maker's vault (`~/Aurelia/vault`, or `AURELIA_VAULT`) and, for
the report's numbers, Nuome's `nuome` (`~/Nuome/target/release`, or `NUOME`).

## The seven steps

1. **CHOP** ([src/chop.rs](src/chop.rs)). Windows' Media Foundation
   decodes the movie (MP4, MOV and what else Windows plays) and hands out
   every frame, as 8-bit YUV 4:2:0. A GIF is read by the gif reader and
   drawn frame over frame, as RGBA. Checked from outside on the test
   clips: the frames are byte for byte the ones ffmpeg decodes. A frame
   the file itself hides (an edit list) is chopped too.
2. **GRID.** Every frame is a tile of one grid, the smallest square-ish one
   with a tile for each frame, filled row by row. A node knows its tile's
   column and row. The grid is never laid out as one picture in memory:
   two seconds of 4K are a 30 720 x 17 280 grid.
3. **PACK** ([src/pack.rs](src/pack.rs), [src/motor.rs](src/motor.rs)).
   A tile stands on its own, or leans on the tile before it on the grid:
   then only what differs from that tile is packed, and it unpacks after
   it. Every frame is still kept and comes back byte for byte. **HotMotor**
   chooses how, per tile: one rule for what the tile leans on (own, tile),
   one for what a pixel is guessed from (none, left, up, paeth, med), one
   for the coder (deflate, zstd, xz), a beam
   over them, and a size judge that packs a sample of the tile itself with
   each choice. Every number is in [rules.toml](rules.toml). `--explain`
   shows the finalists for the first tile.
   **Strike's lightning** brands every pack by the hash of what decides it
   (the frame, the pack rules, the profile, the particle species): a tile
   packed before is struck from Nimbus and not packed again.
4. **NODES** ([src/particle.rs](src/particle.rs)). A node is a particle: one
   copy of a species the rice bowl's particle maker made
   (`Aurelia/particle_maker`, read from its vault). The particle's ports,
   in the order its grains' contacts began, hold the packed tile, one share
   each. The bond from one copy's port to the next copy's port is the link
   to the next frame; it is the species' strongest attracting pair of
   ports (affinity s = -q_a . q_b), the pair by which copies of it bind.
   The species chosen is the one in the vault whose copies bind hardest.
   A film is a chain of bonds through every node once.
   *A particle carries the data; it does not make it smaller.*
5. **GLUE** ([src/bottle.rs](src/bottle.rs)). The shares lie in Strike's
   **Nimbus**, each once, named by the SHA-256 of its content. They are
   struck from there into the one file, checked by name on the way, and
   the index (grid, species, nodes) is written after them. Strike's
   `lightning.rs` and `fstrike.rs` are compiled in as they are in Strike.
6. **PLAYER** ([src/player.rs](src/player.rs)). The nodes are loaded into
   memory. The frames live in Nimbus in a **rolling window** around the
   playhead: the nodes a little ahead are unpacked from their particles
   just before they play, and what the playhead has left behind leaves
   Nimbus. A frame already in the window is struck from it. The frames
   are made ready on one thread and shown by another, each at its time, in
   the player's own window ([src/screen.rs](src/screen.rs)).
7. **SMOOTHER** ([src/smooth.rs](src/smooth.rs)). Between node A and node B
   the frame is cut into blocks, and for each block a vector d is found
   such that A at p - d and B at p + d look most alike. A guessed frame at
   k/n of the way mixes A at p - 2d k/n and B at p + 2d (n-k)/n. Guessed
   frames are made live, just before they play, kept only in the window,
   never stored in a bottle and never counted as frames of the movie.

The report's numbers (grid, ratios, frame rates) are worked out exactly by
Nuome's **Golden Answer** in its logical kind ([src/golden.rs](src/golden.rs)).

## Measured (2026-11-14, this machine, 12 threads)

| clip | frames | original | Frame Bottle | original / bottle | unpacked frames / bottle | FPS | playback FPS | frames identical |
|---|---|---|---|---|---|---|---|---|
| `guitar_3s.mp4`, 1920x1080 H.264 | 91 | 4 719 527 | 50 721 120 | 0.093 | 5.58 | 30 | 60 | yes, 91 of 91 |
| `bbb_4k_2s.mp4`, 3840x2160 H.264, `--for size` | 62 | 1 730 468 | 203 426 512 | 0.0085 | 3.79 | 30 | 60 | yes, 62 of 62 |
| the same, `--for play` | 62 | 1 730 468 | 206 525 522 | 0.0084 | 3.74 | 30 | 60 | yes, 62 of 62 |
| `skeletonplay.gif`, 512x512 | 8 | 87 541 | 26 355 | 3.32 | 318 | 25/3 | 50/3 | yes, 8 of 8 |

Sizes in bytes. "Frames identical" compares every node, unpacked from the
bottle, byte by byte with the movie's frames chopped again by ffmpeg.

**A bottle of a movie that is already compressed is much bigger than the
movie**: 11 times for the 1080p clip, 118 times for the 4K one. H.264
keeps only what changes between frames and throws detail away; a bottle
keeps every frame whole and exact. Against the frames themselves a bottle
is 3.7 to 5.6 times smaller. The GIF is the other way round: its frames
are flat drawings, and the bottle is a third of the GIF.

How long one step along the film takes (frame to frame, node or guessed),
with nothing in the window at the start:

| bottle | ms a frame | enough for | needed for playback |
|---|---|---|---|
| guitar 1080p | 5.0 | 202 frames a second | 60 |
| 4K, `--for play` | 14.3 | 70 | 60 |
| 4K, `--for size` | 21.9 | 46 | 60: **not reached** |
| GIF | 1.8 (0.1 when the window holds it) | 550 | 50/3 |

Packing takes 0.3 s a frame at 1080p and 1.5 s a frame at 4K; a clip packed
before takes 0.4 s (struck).

The smoother, checked by hiding every second frame and guessing it from its
neighbours (`fbottle smoother`): 38.89 dB with vectors against 36.46 dB for
a plain mix on the 4K clip (nearer in 24 of 24), 34.11 dB against 33.90 dB
on the guitar clip. Its numbers were tuned on these two clips.

**Leaning on the tile before** (added after the table above was measured)
made almost no difference on filmed or rendered video: the 1080p bottle
went from 50 721 120 to 50 571 840 bytes (5 of 91 tiles chose to lean), the
4K one did not change (no tile chose to), the GIF went from 26 355 to
25 714. The grain of a picture is new in every frame, and what is kept
exactly has to keep the grain.

**Without ffmpeg** (added last): the 4K clip was cut again so that it has
no hidden frames, and has 69 frames now. With `--for play` its bottle is
230 860 923 bytes, all 69 frames unpack identically and are the ones ffmpeg
decodes, and a step takes 13.8 ms (72 frames a second). The 1080p clip is
unchanged: 5.0 ms a step.

## What it is not, yet

- No sound: a bottle holds the frames only.
- A frame rate that changes inside a movie is played at its average.
- 10-bit movies are not chopped yet (the packer and the player know the
  layouts; the chopper asks Windows for 8-bit frames).
- The window keeps whole frames in Nimbus: about 95 frames at most, which
  is 1.2 GB for 4K. The packed shares of every bottle made also stay in
  Nimbus (`~/.strike/nimbus`) until they are removed there.
- The vectors are whole pixels per 16-pixel block, so a guessed frame can
  show block edges where things move fast or turn.
- It was not built with Strike itself; cargo built it.

The clips are not in the repository. The 4K one is two seconds of *Big Buck
Bunny* (c) Blender Foundation, CC BY 3.0, peach.blender.org.

## Guess the frame (the lossless packer, added 2026-11-14)

HotMotor's rule `from` has a third variant, `guess`
([src/guess.rs](src/guess.rs)); with `--for size` HotMotor chooses it for
every frame of filmed video.

1. **GUESS.** A frame is cut into tiles of 16 x 16 pixels. Each sample is
   guessed from what the player will have too: the frame before, whole,
   and the samples of this frame already rebuilt to the left and above.
   Three guessers offer a value: `still` (the same sample of the frame
   before), `moved` (the frame before, changed as the neighbours changed)
   and `near` (this frame's neighbours alone; the first frame has only
   this). A judge weighs them by how wrong each was on the four
   neighbours just rebuilt, and the guess is their mix.
2. **CORRECT.** Only what the real sample differs by is kept. A tile whose
   every correction is zero keeps one flag. A frame equal to the frame
   before keeps eleven bytes.
3. **PACK.** The corrections go through an adaptive range coder that
   learns, per context (how large the corrections nearby were, which
   guesser was best), how likely a zero, a sign and a size are. The bytes
   lie in Strike's Nimbus, and Strike's lightning brands the pack. The
   coder itself is Frame Bottle's own code: Strike has none.
4. **UNPACK.** The player makes the same guesses and adds the corrections.
   Every frame is checked against the SHA-256 of the frame as chopped. A
   plane is cut into 16 bands of rows that are rebuilt side by side.

Nuome's Golden Answer works out the report's numbers. It takes no part in
the guess: it cannot be asked once for every sample.

Measured on `guitar_10s.mp4` (1920 x 1080, 302 frames, 10 seconds; the
other encodes were made with ffmpeg outside Frame Bottle):

| | bytes | bits per pixel | every frame exact |
|---|---|---|---|
| raw RGB | | 24 | |
| raw YUV 4:2:0 (the movie's own layout) | 939 340 800 | 12 | |
| FFV1 | 141 083 635 | 1.802 | yes |
| **Frame Bottle, guess** | **129 237 300** | **1.651** | **yes, 302 of 302** |
| AV1 lossless (libaom) | 119 672 759 | 1.529 | yes |
| H.264 lossless (x264) | 102 477 615 | 1.309 | yes |
| the MP4 itself (H.264, lossy) | 15 281 386 | 0.195 | |
| AV1, lossy (SVT-AV1, crf 30) | 2 159 426 | 0.028 | no |

- Frames guessed perfectly: 0 of 302. Tiles guessed perfectly: 1 544 296
  of 7 392 960 (20.9 %; a tile is 16 x 16 pixels of light or 8 x 8 of
  colour).
- Every frame unpacks identically (SHA-256 per frame, and byte for byte
  against the frames ffmpeg decodes).
- Packing: 0.1 s a frame. A step along the film at twice the frame rate:
  about 8.6 ms with nothing in the window (the timings vary from run to
  run, 6.5 to 11.6 ms were seen).
- The guessers read the frame before only at the same place: nothing
  follows motion yet. That is the main thing the two that beat it do.

### Golden Answer as a guesser (added after the table above)

A fourth guesser, `golden`: the frame before, made lighter or darker by
one exact fraction per plane. When a frame is packed, Nuome's Golden Answer
is asked, in its logical kind, what the light of the plane is over the
light of the same plane before ("What is 219408113 / 219377412?"); its
answer, a fraction in lowest terms, is kept in the bottle (8 bytes a
plane), so the player makes the same guess without Nuome. Golden's guess
counts only where it differs from `still`, since otherwise `still` would
be heard twice. This replaces the sentence above that Golden takes no part
in the guess.

Measured:

| clip | without Golden | with Golden |
|---|---|---|
| `guitar_10s.mp4` (steady light) | 129 237 300 | 129 246 961 (the kept fractions cost 9 661 bytes; the guess gains nothing) |
| `fade_2s.mp4` (the guitar clip fading to black, 60 frames) | 18 205 507 | 18 131 561 (0.4 % smaller) |

Every frame still unpacks identically in both. Golden's guesser helps only
where the light changes, and only a little, because `moved` already
follows what the neighbours did. Packing asks Golden three questions a
frame (about 22 ms each).

### The Hammer (added last)

Every frame so far lies on the others, so each pixel is one line of
values through time: the top frame first, then the next and the next
until the last. For each pixel the Hammer's stack keeps where its line has
settled; each new frame moves it a quarter of the way to the pixel's new
value. Nothing in the stack is guessed. The fifth guesser, `hammer`, reads
the stack: a pixel that stays what it was is guessed right from its line,
and the grain of one frame does not shake that guess as it shakes `still`.
Like Golden, it counts only where it says something else than `still`.

The player lays the same frames on its own stack in the same order, so a
guessed frame now needs every frame before it, not only the last one
(`Chain` in [src/bottle.rs](src/bottle.rs)).

Measured on `guitar_10s.mp4`: 128 886 416 bytes, 1.6465 bits per pixel
(129 246 961 and 1.6511 without the Hammer: 0.28 % smaller); 1 547 410 of
7 392 960 tiles guessed perfectly; all 302 frames identical. A quarter of
the way was the best of a half, a quarter and an eighth. A step along the
film takes about 12.2 ms now (about 10 before): five guessers are asked
for every sample.
