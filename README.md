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

### A tolerance: the wrong guesses that are left as they are

`--tolerance N`: a guess that is wrong by N or less is not corrected, and a
larger miss is corrected to within N. No sample of any frame is further
than N from the movie's (checked by `verify`). The guesses that follow are
made from the samples as the player will have them, so nothing drifts;
each frame is packed after the one before it. With `--tolerance 0` (the
default) nothing changes: every frame comes back exactly. Leaving every
wrong guess uncorrected is not offered: a bottle would then hold nothing of
any frame after the first.

Measured on `guitar_10s.mp4` (PSNR over all samples, against the MP4's own
frames):

| | bytes | furthest sample | PSNR |
|---|---|---|---|
| tolerance 0 (exact) | 128 886 416 | 0 | exact |
| H.264 lossless | 102 477 615 | 0 | exact |
| tolerance 1 | 69 837 647 | 1 | 50.88 dB |
| tolerance 2 | 46 700 754 | 2 | 47.20 dB |
| tolerance 4 | 29 556 629 | 4 | 42.74 dB |
| tolerance 8 | 17 990 116 | 8 | 37.28 dB |
| the MP4 itself (H.264) | 15 281 386 | | (it is the reference) |
| tolerance 12 | 13 023 181 | 12 | 33.93 dB |
| AV1 (SVT-AV1, crf 30) | 2 159 426 | | 40.56 dB |

With a tolerance of 1 a bottle is smaller than every lossless encode. At
the quality of the AV1 file (about 40 dB) a bottle is about ten times its
size: a tolerance spends its bits sample by sample, where AV1 describes a
block of 64 or more samples with a few numbers.

### Mending what a tolerance lost

The player mends a frame rebuilt with a tolerance before it shows it
(`guess::mend`): each sample becomes the mean of itself and the samples
around it that are the same surface (within 2 N of it), held inside
N of what was rebuilt. It costs no byte: the bottle is the same, and the
guesses are still made from the frames as rebuilt. `verify` reports both.

Measured on `guitar_10s.mp4`:

| tolerance | bytes | PSNR as rebuilt | PSNR as shown (mended) | furthest sample as shown |
|---|---|---|---|---|
| 1 | 69 837 647 | 50.88 dB | 51.31 dB | 2 |
| 2 | 46 700 754 | 47.20 dB | 48.08 dB | 4 |
| 4 | 29 556 629 | 42.74 dB | 43.68 dB | 8 |
| 8 | 17 990 116 | 37.28 dB | 38.10 dB | 16 |
| 12 | 13 023 181 | 33.93 dB | 34.65 dB | 24 |

Mending brings the frames 0.4 to 0.9 dB nearer on average. A single sample
can end up further than before (up to 2 N), because the mean is a guess.
The Hammer's stack was tried as a second witness and made it worse (it is
made of rebuilt frames, whose misses lean the same way). A step along the
film takes about 15 ms with mending.

## Blocks: against AV1 (`--coarse N`, added 2026-11-14)

[src/block.rs](src/block.rs) keeps a frame the way AV1 and H.264 do, in
its simplest form. A frame is cut into the same 16 x 16 tiles. For each
tile the packer looks for the same picture in the frame before, as the
player rebuilt it, up to 7 pixels away; if it is there, only the vector is
kept. If not, the tile is guessed as one flat value from the pixels above
and to its left. What the guess misses is turned, 8 x 8 samples at a time,
into 64 waves (a cosine transform in whole numbers, so that every player
gets the same); each wave is counted in steps of length N (a little longer
for finer waves) and a wave shorter than a step is left out. The steps
left are written with the adaptive range coder. Every number is in
[rules.toml](rules.toml) under `[block]`. Only 8-bit YUV 4:2:0 is cut this
way. Frames are not exact; `verify` reports how near they are.

Measured on `guitar_10s.mp4` (1920 x 1080, 302 frames). PSNR is over all
samples against the MP4's own frames, for every row; the other encodes
were made with ffmpeg outside Frame Bottle (x264 preset medium, SVT-AV1
preset 8):

| | bytes | PSNR |
|---|---|---|
| Frame Bottle, exact | 128 886 416 | exact |
| the MP4 itself | 15 281 386 | the reference |
| Frame Bottle, tolerance 4 | 29 556 629 | 42.74 dB |
| Frame Bottle, blocks, coarse 6 | 15 890 099 | 45.67 dB |
| Frame Bottle, blocks, coarse 10 | 8 863 029 | 42.88 dB |
| **Frame Bottle, blocks, coarse 15** | **5 864 947** | **41.03 dB** |
| H.264, crf 22 | 4 621 893 | 40.66 dB |
| Frame Bottle, blocks, coarse 22 | 4 065 939 | 39.01 dB |
| AV1, crf 24 | 3 394 188 | 40.70 dB |
| Frame Bottle, blocks, coarse 32 | 2 980 600 | 37.24 dB |
| H.264, crf 26 | 2 334 877 | 40.05 dB |
| AV1, crf 30 | 2 159 426 | 40.56 dB |
| AV1, crf 36 | 1 450 315 | 40.37 dB |
| H.264, crf 30 | 1 313 941 | 39.15 dB |

- Against the tolerance: at about 42.8 dB blocks take 8.9 MB where a
  tolerance takes 29.6 MB.
- Against H.264 at about 40.7 dB: a bottle is about 5.5 MB, H.264 4.6 MB.
- Against AV1 at about 40.6 dB: a bottle is about 5.5 MB, AV1 2.2 MB
  (2.5 times smaller) and still 40.4 dB at 1.5 MB.
- Both of them lose almost nothing when they are squeezed (40.7 to
  39.1 dB from 4.6 to 1.3 MB); a bottle loses fast (41.0 to 37.2 dB from
  5.9 to 3.0 MB). Below about 40 dB a bottle is far behind.
- Packing takes 8 seconds for the 10 seconds; a step along the film about
  5 ms.

What AV1 has that this does not, most telling first: vectors to a part of
a pixel, guesses of a tile from its neighbours in many directions (here
one flat value), a filter that smooths block edges inside the loop, tiles
of many sizes, frames guessed from a later frame as well, and a packer that
weighs bytes against loss for every choice.

### Blocks, second round, and a correction to the table above

**The PSNR of H.264 and AV1 in the table above is wrong.** It was taken
with ffmpeg's `psnr` filter, which pairs frames by their time; this clip's
frame times are uneven, so some frames were compared with the wrong frame
and the two looked about 5 dB worse than they are. Measured frame by frame
in decoding order (the way `fbottle verify` counts), they are far ahead:

| | bytes | PSNR |
|---|---|---|
| Frame Bottle, blocks, coarse 5 | 17 602 274 | 47.58 dB |
| Frame Bottle, blocks, coarse 8 | 9 402 634 | 45.44 dB |
| Frame Bottle, blocks, coarse 12 | 6 372 995 | 43.89 dB |
| H.264, crf 22 | 4 621 893 | 46.33 dB |
| Frame Bottle, blocks, coarse 18 | 3 972 884 | 41.75 dB |
| AV1, crf 24 | 3 394 188 | 46.33 dB |
| Frame Bottle, blocks, coarse 26 | 2 886 012 | 40.05 dB |
| H.264, crf 26 | 2 334 877 | 44.23 dB |
| AV1, crf 30 | 2 159 426 | 45.76 dB |
| Frame Bottle, blocks, coarse 40 | 2 133 715 | 37.92 dB |
| AV1, crf 36 | 1 450 315 | 45.09 dB |
| H.264, crf 30 | 1 313 941 | 41.97 dB |

At about 45.4 dB a bottle is 9.4 MB, H.264 about 3.3 MB and AV1 about
1.7 MB: H.264 is about 3 times smaller, AV1 about 5.5 times.

What changed in this round (each measured on the clip):

- **Seams smoothed inside the loop.** The seams between 8 x 8 blocks are
  smoothed when a frame is rebuilt, and the smoothed frame is what the next
  one is found in; only seams of blocks made anew in that frame.
  Coarse 22: 39.0 to 40.7 dB, 4 % smaller.
- **Vectors to half a pixel.** Whole pixels first, then the eight
  half-pixel places around the best. A little nearer at the same size.
- **The frame before is preferred to a flat guess.** The packer had
  compared the frame before with the tile's own mean, and took "flat" for
  four tiles in ten of a picture that stands still; the quality sank from
  42 to 36 dB along the film. Now a flat guess is taken only when it is far
  nearer. Coarse 45: 36.8 to 37.9 dB at the same size.
- **A tile counts as moved only when that is clearly nearer** than standing
  still (`still` in rules.toml). A quarter fewer bytes for half a dB.
- **The top frame is kept three times finer**: every still tile after it is
  a copy of it.

Together: about 41 dB took 5.9 MB before this round and takes about 3.5 MB
now. Tried and dropped, because they made it worse: looking 20 pixels away
instead of 7, a stronger preference for standing still, and leaving out
blocks with only one or two small waves.

### The Hammer's stack as a place to take a tile from

A tile can now be taken from three places: the frame before (by a vector),
a flat guess, or the Hammer's stack (every frame so far, one on the other,
at the same place). The packer takes it from the stack when the stack is
more like the tile than the frame before by a clear margin.

Measured on `guitar_10s.mp4`: no gain. Coarse 8: 9 400 423 bytes at
45.44 dB (9 402 634 before); coarse 18: 3 971 804 at 41.75 dB (3 972 884).
A stack that settles more slowly (1/16, 1/32 of the way a frame) was tried
too: 0.03 dB nearer for 0.4 % more bytes. Where the picture stands still, a
tile is already a copy of the tile before it, miss and all, so every frame
in the stack carries the same miss and stacking them evens nothing out.

### How long a pixel has stood still

Measured on `guitar_10s.mp4`: 58.3 % of the samples of a frame are exactly
what they were in the frame before (85.0 % within 1, 94.1 % within 4), but
they lie scattered: only 2.7 % of the 16 x 16 tiles are the same all over.
Which samples stay is what costs: with nothing known, 0.98 bits a sample.
A sample's own line through the frames helps (one that has stood still
for 8 frames stays again 89 times in 100, one that just changed 37 times
in 100): 0.855 bits. Its line and its two neighbours together: 0.578 bits.

The Hammer's stack now keeps, for each sample, how many frames in a row it
has stood still, and the coder is told whether that is 8 or more.
Result: 0.03 % smaller on the 3 s clip (40 371 157 to 40 359 977 bytes).
The coder already knew most of it from the neighbours and from which
guesser was right. Telling it in 6 steps made the bottle 0.2 % bigger
(too many cases to learn each from one band), and letting the judge hear
`still` louder the longer a sample has stood still made it 1.9 % bigger.

### `--line`: each sample first says whether it stayed

Built to get the 0.578 bits a sample measured above: with `--line`, a
frame says for every sample whether it is exactly what it was in the frame
before, and the coder learns that from how long the Hammer has seen the
sample stand still and from whether the samples to its left, above it and
above to the right stayed. Only a sample that did not stay has a
correction. Frames still unpack identically.

Measured on `guitar_3s.mp4`: 47 302 091 bytes with `--line`, 40 359 977
without: 17 % bigger. HotMotor itself turned it down for 39 of the 91
frames (plain `own+med+xz` was smaller there). The 0.578 bits are the
price of the map alone; the 42 % of samples that changed still need their
corrections on top, and they are the costly ones. Without `--line` there
is no map: one question, "was the mixed guess right?", covers both, and
the mixed guess is right for many samples that did change. `--line` is
left in as a switch, off unless asked for.

`--line` with a tolerance (a sample within the tolerance of what it was in
the frame before stays exactly that) was measured too, on `guitar_10s.mp4`:
bigger and further from the movie at every tolerance (tolerance 4:
49 143 713 bytes at 40.40 dB, against 29 607 960 at 42.74 dB without it).
A sample that stays is held at an old value until it is a whole tolerance
off, so the frames the next guesses are made from are worse.

## The pattern dictionary (`--dictionary`, added 2026-11-15)

Two dictionaries, both self-updating, capped, and never stored in the
bottle: the packer and the player build them in the same order from what
both have already seen.

**On the bits of the corrections** ([src/dict.rs](src/dict.rs), as
designed). After a frame's corrections are written, their bits are read
again a byte at a time. Patterns of 2 to 8 bytes are kept in a dictionary
of at most 4096; a pattern enters when two pieces have been seen one after
the other twice, the most used have the lowest ranks (the shortest labels),
and when it is full the one not used for the longest is dropped. A frame is
kept through the dictionary only when that is smaller. Every frame so kept
is checked against 8 bytes of the SHA-256 of the bits it was made from, and
the dictionary as it stood after the last frame lies in Nimbus under its
hash, which the bottle's index holds: `verify` fails if the player's
dictionary ends otherwise.

**On the corrections themselves, before the coder** (`Book` in
[src/block.rs](src/block.rs)). The same rules, but a pattern is a whole
8 x 8 block's corrections (which waves are left and their steps). A block
whose corrections are in the book is kept as the pattern's rank. One book
of at most 1024 patterns for each band, carried from frame to frame.

Measured on `guitar_10s.mp4`, blocks at coarse 20 (41.18 dB both ways: the
dictionaries lose nothing):

| | bytes |
|---|---|
| before | 3 598 135 |
| after (`--dictionary`) | 3 493 256 (2.9 % saved) |
| AV1 at about the same quality (SVT-AV1 crf 60, 40.86 dB) | 258 585 |
| AV1, crf 55 (42.48 dB) | 435 215 |
| H.264, crf 30 (41.97 dB) | 1 313 941 |

- **The bit dictionary saved nothing.** Its hit rate was 6.43 % of the
  pieces (12.4 % of the bits), and not one of the 302 frames came out
  smaller through it: all of them through it would be 2.1 % bigger. The
  bits come out of the range coder, which has already taken every pattern
  out of them; 6.4 % is what 4096 patterns of two bytes hit by chance
  (4096 of 65 536 is 6.25 %), and a label costs more than the 16 bits it
  stands for. The same on the exact packing: 6.25 %, no frame smaller.
- **The book of block patterns saved the 2.9 %.** 76.3 % of the blocks that
  have corrections were in a book. At coarse 8 (45.44 dB): 9 400 443 to
  9 239 961 bytes, 1.7 % saved, 68.4 % in a book. A book of 256 saves 2.2 %,
  one of 4096 saves 3.1 %.
- The player rebuilds the same bits: the dictionary's end hash matched, and
  every frame rebuilt from the books has the SHA-256 the packer recorded.
- Against AV1 at the same quality a bottle is still about 12 times bigger.

### Four ready-made patterns (`--four`)

The book of block patterns with only four patterns, put in ready-made and
never added to: the flattest wave one step down, one step up, and the next
wave one step down, one step up. They are the four commonest on
`guitar_10s.mp4` at coarse 20: 19.5 %, 9.7 %, 3.2 % and 3.1 % of the blocks
that have corrections (35.5 % together).

| coarse 20, 41.18 dB | bytes | saved |
|---|---|---|
| no dictionary | 3 598 135 | |
| four ready-made patterns | 3 555 361 | 1.2 % |
| the learning book (1024 a band) | 3 493 256 | 2.9 % |

Not 30 %. The four patterns are a third of the blocks with corrections,
but they are the cheapest third: a block with one wave one step long took
only a few bits before, since the coder had already learned that such
blocks are common. The bytes are in the blocks with many waves, and those
are all different.

### The dictionary before the final packing (`--dictionary-first`)

With `--coarse` and `--dictionary-first` the corrections are not coded at
all at first: every yes/no of a frame (is the tile moved, is a wave left,
each digit of a step) is written as one plain bit. The pattern dictionary
of [src/dict.rs](src/dict.rs) then goes through those bits, and what it
writes (labels and the bytes in no pattern) is the final packing.
`--patterns N` sets how many patterns it may hold (4096; at most 32 767,
which is what a label can name).

Measured on `guitar_10s.mp4`, coarse 20, 41.18 dB in every row; the frames
rebuilt are the same, and the player's dictionary ended as the packer's:

| | the frames' bytes | the bottle |
|---|---|---|
| the corrections as plain bits, nothing else | 6 035 976 | |
| dictionary first, 4 patterns | 4 614 984 | 4 794 030 |
| dictionary first, 4096 patterns | 4 147 857 | 4 326 749 |
| dictionary first, 32 767 patterns | 3 971 963 | 4 150 740 |
| the coder (as before, no dictionary) | 3 314 639 | 3 598 135 |
| the coder with the book of block patterns | | 3 493 256 |

- Here the dictionary does find patterns: with 4096 of them 52.6 % of the
  pieces are labels, covering 75.5 % of the bits; with 32 767, 79.9 % and
  91.8 %. The commonest is "1 and eight 0s", over and over: a tile that
  stands still (moved: yes, vector: none, six blocks with no wave left).
- Against the plain bits it saves 31 % (4096 patterns) to 34 % (32 767).
- Against the coder it replaces it is 25 % bigger (4096) or 20 % (32 767):
  the coder learns for each yes/no how likely it is where it stands (which
  wave, what the neighbour did), and a pattern of whole bytes cannot.
- With only 4 patterns almost nothing is a label (3.2 % of the pieces);
  what it saves comes from the bytes themselves being uneven.
- A step along the film: 5.6 ms (plain bits are quick to read).

Seen on the way: the bottle is about 280 000 bytes bigger than its frames'
bytes at this size (8 %). That is the index: for each of the 3322 shares
its SHA-256 as 64 letters, its place and length, and each node's ports.
