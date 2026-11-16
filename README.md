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

### Vectors to a quarter of a pixel, with H.264's filter (2026-11-15)

The goal now: over 45 dB, and smaller than AV1 there (AV1 is about 1.7 MB
at 45.4 dB on `guitar_10s.mp4`).

A tile's vector is now counted in quarters of a pixel. Half way between two
pixels is six pixels in a row weighed 1, -5, 20, 20, -5, 1, and a quarter
is the mean of the two places next to it: H.264's own filter, since a movie
that was H.264 once moved its blocks with exactly this. Colour planes take
the four pixels around the place, weighed by how near each is. The search
goes whole pixels, then half, then quarter, each finer step kept only if it
is clearly nearer; it works on a cut-out of the frame before, so packing
takes 11 s for the 10 s clip as before.

| `guitar_10s.mp4` | half pixel | quarter pixel |
|---|---|---|
| coarse 8 | 9 400 443 bytes, 45.44 dB | 7 839 470 bytes, 45.48 dB (16.6 % smaller) |
| coarse 20 | 3 598 135 bytes, 41.18 dB | 3 209 251 bytes, 41.47 dB (10.8 % smaller, 0.3 dB nearer) |

## Toward AV1 at over 45 dB (2026-11-15)

The goal: over 45 dB, and smaller than AV1 there. All on `guitar_10s.mp4`;
PSNR is over all samples frame by frame in order, VMAF is ffmpeg's libvmaf
on the frames as the player shows them (`fbottle raw`), against the MP4's
own frames.

What was added to the blocks, each measured:

| step | effect |
|---|---|
| the index kept tight (8 bytes of each share's name, the nodes as a table) | 128 kB less: 4.1 % of a 3 MB bottle, 1.7 % of a 7.7 MB one |
| the packer weighs every way of guessing a tile (bits against error); a tile can be split in four, each quarter with its own vector | coarse 8: 7.71 MB at 45.48 dB to 7.51 MB at 46.24 dB |
| `--between`: every second frame guessed from the frames on both sides, its steps 1.7 times longer | about 14 % smaller at the same PSNR |
| waves written by the size of the waves next to them (AV1's way) | 2.5 % smaller, same frames |
| each wave weighed on its own (AV1's way) | nothing: the dead zone already rounds a wave down unless it is nearly a step long |

Where it stands:

| | bytes | PSNR | VMAF |
|---|---|---|---|
| Frame Bottle, `--coarse 8 --between --dictionary` | 5 859 483 | 45.94 dB | 94.58 |
| Frame Bottle, `--coarse 9 --between --dictionary` | 5 144 164 | 45.35 dB | 93.85 |
| H.264, crf 22 | 4 621 893 | 46.33 dB | 94.90 |
| H.264, crf 26 | 2 334 877 | 44.23 dB | 91.88 |
| AV1, crf 30 | 2 159 426 | 45.76 dB | 94.59 |
| AV1, crf 36 | 1 450 315 | 45.09 dB | 93.51 |
| AV1, crf 48 | 708 735 | 43.70 dB | 91.02 |

At VMAF 94.6 a bottle is 5.86 MB and AV1 2.16 MB: AV1 is 2.7 times smaller
(it was 5.5 times at the start of the day). H.264 is about a quarter
smaller than a bottle there. A step along the film: about 8 ms.

### Dimmed light, a grid for each frame, fake grain

[src/light.rs](src/light.rs). With the exact packing; the brightness plane
is kept as level numbers and the player brightens it back, smoothing each
sample toward its neighbours inside the span its level stands for (against
banding; noise laid on before packing would make the frames harder to pack).

| | bytes | PSNR | VMAF |
|---|---|---|---|
| exact (256 levels) | 128 825 550 | exact | 100 |
| `--light 128` (on the gamma curve) | 88 129 586 | 50.81 dB | 98.52 |
| `--light 96` | 78 412 143 | 48.75 dB | 97.95 |
| `--light 64` | 66 282 836 | 45.24 dB | 96.44 |
| `--grid 128` (a grid made for each frame) | 105 136 983 | 53.85 dB | 98.00 |
| `--grid 64` | 85 578 508 | 48.80 dB | 94.96 |

- Dimmed light makes the exact bottle a third to a half smaller. Against
  the blocks it is far off: 66 MB at 45.2 dB where blocks take about 5 MB.
- A grid for each frame is nearer the movie level for level (48.8 dB with
  64 levels, where the curve needs 96), but its bottle is bigger than the
  curve's at the same PSNR (85.6 MB against 78.4 MB) and its VMAF is lower
  (94.96 against 97.95): when a frame stores a new grid, every level number
  means something else, and the guess from the frame before misses.
- `--grain` (fake film grain on what is shown, three quarters of the
  strength of what the packing left out): the bottle is the same size; PSNR
  falls from 45.94 to 44.17 dB and VMAF from 94.58 to 94.36, as they must
  when grain that is not the movie's own is laid on. Whether it looks
  better is for the eye, not for these two numbers.

### Groups of frames (2026-11-15, later)

Where the bytes were, coarse 8 with one frame between two: the first frame
277 kB, each frame guessed from the one before 27.9 kB, each frame between
two 8.8 kB. So frames are now packed in groups of four (`group` in
rules.toml): the last of a group is guessed from the last of the group
before it, the middle one from those two, the rest between their two
nearest. A tile of a frame between two can come from the frame before,
from the frame after (AV1's choice among its frames), or from both mixed.
A frame others are guessed from has steps the square root of `between`
longer, a frame nothing is guessed from `between` (2.1) times longer. The
player rebuilds the frames in the packer's order, each when it is first
needed, and keeps the last few at hand.

| `guitar_10s.mp4`, `--between` | bytes | PSNR | VMAF |
|---|---|---|---|
| groups of 2, a tile from before or from both (as it was) | 5 882 135 | 45.94 dB | 94.58 |
| groups of 2, a tile also from the frame after | 5 680 574 | 46.09 dB | |
| groups of 4, steps 1.7 times longer | 5 269 614 | 45.94 dB | |
| groups of 8 | 5 277 455 | 45.74 dB | |
| **groups of 4, steps 2.1 times longer, coarse 8, with the book** | **4 852 084** | **45.71 dB** | **94.33** |
| the same, coarse 7 | 5 600 295 | 46.28 dB | 94.97 |
| AV1, crf 30 | 2 159 426 | 45.76 dB | 94.59 |
| H.264, crf 22 | 4 621 893 | 46.33 dB | 94.90 |

Groups of eight were worse than groups of four (the frames in the middle
lie too far from their two), and before a tile could come from the frame
after alone, groups of four were no better than groups of two. At VMAF 94.6
a bottle is now about 5.15 MB against AV1's 2.16 MB (2.4 times); H.264 is
about 16 % smaller than a bottle at VMAF 94.9. A step along the film:
about 10 ms. The book of block patterns saves only 0.3 % now.

### Four more of AV1's ways (2026-11-15, last)

| what | measured on `guitar_10s.mp4`, coarse 8, `--between` |
|---|---|
| a tile that is in no other frame guessed from the pixels around each block, four ways (their mean, the row above carried down, the column to the left carried across, Paeth) | the first frame 276 806 to 268 971 bytes (2.8 %) |
| the rebuilt frame's levels put right inside the loop (32 spans of 8 levels in each plane, by how far off they were on the whole; 96 bytes a frame) | with the next: |
| the whole picture's move (the commonest vector of every third tile), which a tile's vector is told apart from | all three together: 4 868 255 bytes at 45.71 dB to 4 833 022 at 45.74 dB (0.7 %) |
| a tile's light as one block of 16 x 16 when that weighs less than four of 8 x 8 | 4 878 971 bytes at 45.80 dB: 1 % more bytes for 0.06 dB, no gain; it is off (`large` in rules.toml) |

Where it stands (`--coarse N --between --dictionary`):

| | bytes | PSNR | VMAF |
|---|---|---|---|
| Frame Bottle, coarse 8 | 4 830 031 | 45.74 dB | 94.40 |
| Frame Bottle, coarse 7 | 5 566 406 | 46.31 dB | 95.03 |
| H.264, crf 22 | 4 621 893 | 46.33 dB | 94.90 |
| AV1, crf 30 | 2 159 426 | 45.76 dB | 94.59 |

These four moved little. The frames guessed from the one before still take
35 kB each and the frames between two 8 kB; AV1's whole average is 7 kB a
frame.

## The Predictive Motion Engine (`--predict`, 2026-11-15)

**Measured first** (`fbottle pack ... --breakdown` prints it for every frame
and for the whole bottle). The bottle of `--coarse 8 --between`,
4 849 799 bytes:

| | bytes | share |
|---|---|---|
| waves (the corrections) | 3 005 669 | 62.0 % |
| vectors | 1 344 325 | 27.7 % |
| tile kinds and flags | 376 139 | 7.8 % |
| levels put right | 28 992 | 0.6 % |
| lengths, headers, the coder's rounding | 43 108 | 0.9 % |
| the index and the bottle's ends | 51 566 | 1.1 % |
| light grid | 0 | (the blocks keep none) |

In the frames between two, the vectors took more than the corrections.

**The engine** ([src/block.rs](src/block.rs), `Motion`). It remembers the
vector of every tile in the last 4 frames that others are guessed from
(`history` in rules.toml). For each tile four models offer a vector: the
tile to the left (or the whole picture's move); the same tile's vector
last time (constant speed); that carried on by how it changed (constant
change of speed); and, for a frame between two, the part of its group's
vector that falls to it. The model that was nearest for the tile to the
left speaks for this one. Only vectors and frames already rebuilt are
used, in whole numbers, so the player predicts the same; the prediction is
never stored. What is stored is the miss: the vector's difference from the
prediction (often none), and a tile exactly where predicted with nothing
put right is one **skip** flag, learned from the tiles to its left and
above it.

**Cuts.** A frame whose brightness differs from the frame before by more
than `cut` (28 levels on average) stands alone, and the history starts
again there.

**The same on both sides.** Every vector either side predicts is folded
into a check; the bottle's index holds the packer's, and `verify` fails if
the player's differs. A test packs and unpacks frames and compares the two
engines' checks.

Measured on `guitar_10s.mp4`:

| | bytes | PSNR | VMAF |
|---|---|---|---|
| as it was (`--coarse 8 --between`) | 4 849 799 | 45.74 dB | 94.40 |
| **with the engine (`--predict`)** | **4 125 389** | **45.65 dB** | **94.53** |
| with the engine, coarse 7 | 4 885 390 | 46.21 dB | 95.16 |
| with the engine, coarse 6 | 5 836 111 | 46.79 dB | 95.76 |
| AV1 (libaom, cpu-used 5), crf 26 | 1 455 077 | 46.45 dB | 94.99 |
| AV1 (libaom), crf 32 | 903 837 | 45.48 dB | 93.54 |
| AV1 (libaom), crf 38 | 600 212 | 44.56 dB | 91.82 |

15 % smaller, not 3 to 5 times: the vectors fell from 1 344 325 to 759 619
bytes (now 18.4 % of the bottle), the kinds and flags rose a little
(443 841), and the waves, which the engine does not touch, are 2 800 028
bytes: 67.9 % of what is left. 78.0 % of the tiles are skipped, 81.8 % of
the blocks have no wave. At VMAF 94.5 libaom's AV1 is about 1.2 MB: a
bottle is about 3.4 times that.

A second clip with a cut in it (`cut_6s.mp4`: three seconds of one film,
three of another): the cut is found, the frame after it stands alone,
the player's check agrees; 2 337 647 bytes at 45.70 dB without the engine,
2 330 595 bytes at 45.95 dB with it.

### The Neuras refiner: tried and taken out

A small network built and trained with Neuras's own code (three 3 x 3
convolutions of 8 channels, 953 parameters) was to make the motion
engine's guess of a tile better from the same place in the last three
rebuilt frames, in whole numbers on both sides. Trained on 53 406 tiles of
two other guitar films, it did not: on 5 934 tiles kept out the motion
engine's guess was wrong by 8.491 (mean square) and the refined guess by
8.494. The owner's decision: Neuras is left out of Frame Bottle. The code
is in the history (commit 9faccb3).

## Fractal mode (`--fractal N`)

A frame as rules and nothing else: no vectors-and-waves (src/fractal.rs). The frame is
cut into ranges of 16 x 16, split in four down to 4 x 4, and each range is one rule:
`skip` (the same place in the frame before), `moved` (a block of one of the last two
rebuilt frames, its contrast scaled and its brightness shifted), `own` (a domain of the
frame itself, twice the size, turned one of 8 ways: the classic fractal rule) or `flat`.
The player applies the rules 3 times to the frame before (10 times from flat grey for a
frame that stands alone, with `--fractal-keys`; otherwise such a frame is kept as blocks).

- **HotMotor searches the rules**: source, place, turn, contrast and brightness, decision
  by decision; a quick judge and the beam first, then the exact judge (squares of error
  plus N for every bit). The sources' scores are in rules.toml.
- **HotMotor packs the rules**: for each frame it chooses how they are written (vectors
  and shifts plain or told from the rule before) by the bytes that come out.
- **Strike**: the lightning brands each frame's rules, Nimbus keeps them. Packing the
  same clip again: 301 of 302 frames struck, 4.4 s in place of 60 s.

The search runs on the processor (every core), not on the graphics card.

Measured on guitar_10s.mp4 (1080p, 302 frames), from outside, frame by frame:

| bottle | bytes | PSNR | VMAF | packed in |
|---|---|---|---|---|
| `--fractal 500` | 1 405 362 | 35.23 dB | 61.80 | 50 s |
| `--fractal 250` | 1 746 312 | 36.80 dB | 71.06 | 60 s |
| `--fractal 150` | 2 119 391 | 37.89 dB | 76.55 | 50 s |
| `--coarse 8 --between --predict` (the best so far) | 4 125 389 | 45.65 dB | 94.53 | |
| no waves, vectors only (the experiment before) | 2 080 000 | 35.94 dB | 67.2 | |
| libaom AV1 crf 32 | 903 837 | | 93.54 | |
| libaom AV1 crf 26 | 1 455 077 | 46.45 dB | 94.99 | |

The player rebuilds a `--fractal 250` bottle at 8.7 ms a frame (115 frames a second).
Where its bytes are: the frame that stands alone 15.4 %, quadtree flags and kinds of
rule 21.7 %, places and vectors 23.0 %, turns 0.1 %, contrast and brightness 34.8 %.
Of its ranges 77.8 % are `skip`, 20.7 % `moved`, 1.4 % `flat` and 0.1 % `own`: when the
frame before is at hand, a block of it nearly always weighs less than a domain of the
frame itself. `fbottle png <bottle> <frame> <file>` writes a rebuilt frame as a picture.

### The wave (`--wave N`, with `--fractal`)

Devil's Clockwork's wave in the player: when the rules have made a frame, a wave starts
at every seam of a range made anew and moves out a pixel a tick (3 ticks, rules.toml),
weaker each tick; the two sides of a seam pull each other level, all pixels at once. A
step of N or more across a seam is an edge of the picture and is left alone. It costs two
bytes a frame. The packer rebuilds with it, so the next frame is made from what it left.

| bottle (guitar_10s.mp4) | bytes | PSNR | VMAF |
|---|---|---|---|
| `--fractal 250` | 1 746 312 | 36.80 dB | 71.06 |
| `--fractal 250 --wave 12` | 1 717 964 | 36.98 dB | 70.86 |
| `--fractal 250 --wave 24` | 1 693 484 | 36.99 dB | 70.10 |

### Waves weighed by the coder's real bits

The packer weighs each wave (the nearest number of steps, one less, or none) and whether
a block keeps any wave at all with the bits the coder would spend as it has learned so
far (`block_bits` in src/block.rs), in place of fixed guesses (5 bits for a wave gone, 2
for a step less). The player is the same. guitar_10s.mp4, `--between --predict`:

| bottle | bytes | PSNR | VMAF |
|---|---|---|---|
| `--coarse 8`, fixed guesses (before) | 4 125 389 | 45.65 dB | 94.53 |
| `--coarse 8`, real bits | 4 700 703 | 46.17 dB | 95.69 |
| `--coarse 9`, real bits | 4 052 624 | 45.75 dB | 95.22 |
| `--coarse 10`, real bits | 3 608 666 | 45.35 dB | 94.74 |

### `--tolerance 1` against x264 and AV1 at the same nearness

guitar_10s.mp4, measured from outside, frame by frame (the bottle as the player shows it):

| pack | bytes | PSNR | VMAF |
|---|---|---|---|
| Frame Bottle `--tolerance 1` (no sample more than 1 off before mending) | 69 777 286 | 51.31 dB | 98.64 |
| x264 slow crf 6 | 40 671 712 | 56.05 dB | 98.88 |
| x264 slow crf 10 | 26 167 152 | 53.34 dB | 98.55 |
| x264 slow crf 14 | 15 923 385 | 50.75 dB | 97.81 |
| libaom AV1 cpu-used 6 crf 4 | 22 776 015 | 53.27 dB | 98.79 |
| libaom AV1 cpu-used 6 crf 10 | 7 339 883 | 49.79 dB | 97.89 |
| libaom AV1 cpu-used 6 crf 16 | 3 669 642 | 48.17 dB | 96.85 |

## The mix: exact packing with mixed guessers and mixed learners (`--mix`)

src/mix.rs, for 8-bit YUV 4:2:0. Every frame comes back exactly (checked: 302 of 302
frames identical, every sample).

- **Ten guessers mixed** for each sample: MED (LOCO-I), GAP (CALIC), left, above, their
  mean; two learning guessers (a weighed sum of the surroundings whose weights move
  after every sample; with a frame before they also see how it slopes to the right and
  below); and from the frame before, at the place a vector per 16 x 16 block points to:
  the sample, the sample changed as its neighbours changed (median, and mean). Each
  speaks as loud as it was right on the neighbours; whole numbers only. The guess is
  then put right by what guesses in the same surroundings missed by.
- **Five learners mixed** for each yes/no of a miss (is there one, its sign, its size):
  the misses nearby and how far apart the guessers stand; LOCO-I's three slopes; for
  colour the light's miss at the same place (and the first colour's for the second);
  the four neighbours' misses themselves; and where the frame before, the median and
  the learning guesser stand from the guess. Two logistic mixers and a last learner
  make one likelihood of them for the range coder.
- What a band has learned goes on from frame to frame (not with `--alone`).
- `--alone`: every frame stands alone (archive mode); nothing is read from another frame.
- The source is YUV already, so there is no colour transform (YCoCg-R is for RGB).
- Strike: the lightning brands each frame's pack; the same clip again: 302 struck, 1.6 s.

guitar_10s.mp4 (1080p, 302 frames), all exact, all checked against the source:

| pack | bytes | bits a pixel | packing | unpacking |
|---|---|---|---|---|
| **Frame Bottle `--mix`** | **100 545 180** | 1.284 | 33.0 s | 29.9 s (99 ms a frame) |
| x264 lossless (qp 0, veryslow) | 100 839 923 | 1.288 | 14.8 s | 0.8 s |
| FFV1 (level 3, range coder, 16 slices, gop 300) | 124 691 885 | 1.593 | 1.4 s | 1.4 s |
| AV1 lossless (libaom, lossless=1, cpu-used 2) | 124 942 885 | 1.596 | about 730 s (the machine was busy) | 5.1 s |
| Frame Bottle `--mix --alone` | 125 624 819 | 1.605 | 24.4 s | 18.9 s (62 ms a frame) |
| Frame Bottle, exact as before | 128 693 695 | 1.644 | 17.5 s | 5.5 s (18 ms a frame) |
| FFV1 (the same, gop 1: every frame alone) | 134 800 305 | 1.722 | | |

Where the mix's bytes are: light 84.6 %, the first colour 8.6 %, the second 6.0 %,
vectors 0.7 %; by what is said: is there a miss 54.8 %, its sign 18.7 %, its size 25.9 %.

The mix does not play at the movie's speed: 99 ms a frame is 10 frames a second.

### The mix made light, and the strike (2026-11-15, later)

The section above describes the mix as it was first built ("mix 2"). It unpacked at 99 ms
a frame. Two things were done about that; the bottle is still exact (every frame is
checked by its hash as it is rebuilt, and `fbottle raw` gives the source byte for byte).

**1. Where the time went, and what was left out.** Timed stage by stage (24 frames, 12
threads, the clock's own load included): the learners and mixers 38.7 %, the weighing of
the guessers 24.4 %, the frame before and the plain guessers 16.0 %, teaching the
guessers 7.1 %, the learning guessers 5.8 %, the range coder 4.7 %, the contexts 3.3 %.
No colour transform; putting frames in Nimbus was under 1 %. Each part was then left out
alone and the first 24 frames packed again (8 046 559 bytes with everything):

| left out | bytes | |
|---|---|---|
| the three guessers from the frame before (the learning guessers see it anyway) | 8 067 251 | +0.26 % |
| those, and left, above, mean (MED, GAP and the two learning guessers stay) | 8 071 107 | +0.7 % (of 8 012 661, without the last learner) |
| all but the two learning guessers | 8 234 283 | +2.3 % |
| all but the quick learning guesser | 8 373 413 | +4.1 % |
| the last learner (the likelihood put right once more) | 8 012 661 | -0.4 % |
| learner: the misses nearby and how far apart the guessers stand | 8 043 227 | +0.4 % (of 8 012 661) |
| learner: LOCO-I's slopes | 8 046 903 | +0.4 % |
| learner: the light's miss, for colour | 8 019 796 | +0.1 % |
| learner: the four neighbours' misses | 8 064 857 | +0.7 % |
| learner: where the frame before, the median and the learning guesser stand from the guess | 8 351 030 | +4.2 % |
| the quick speed of every learner (the slow one stays) | 8 061 965 | +0.6 % |
| the slow speed of every learner | 8 169 031 | +2.0 % |

The mix now ("mix 3") has four guessers (MED, GAP, the two learning guessers) and three
learners at the slow speed, no last learner; its rows have room around them so that no
sample asks where it stands; the frame before is laid out once for a band where the
vectors point; a band's working room is made once. 24 frames: 8 179 991 bytes (+1.7 %).
The build asks for AVX2 (`.cargo/config.toml`).

**2. The strike** (`mix::strike`, after StrikeS: map everything first, then strike it).
One job is one band of one frame. The map is made from the bottle's index (which frames
lean on the one before, where each frame's bytes lie) and each frame's own head (where
each band's bytes lie): a job waits for the same band of the frame before (what the band
has learned) and, when it reads that frame, for the bands above and below it there. Every
job whose waiting is over is struck at once on every core, and each one that ends lets
the next go. Frames are let in a rolling window (`--window N`, 32) ahead of the last
frame handed over, so the memory stays what the window holds. The packer strikes the
same way (a window of frames at a time; a band's jobs wait only for the band before, as
the real frames are all there). The strike gives the same bytes as packing frame by
frame (tested).

Not done: the bits read in a pass of their own before the samples are rebuilt. The
learners would then see only misses, never samples; tried, 24 frames: 8 901 991 bytes
(+8.8 % over mix 3), and the work a sample takes stays the same.

guitar_10s.mp4 (1080p, 302 frames), Ryzen 5 7600X (6 cores, 12 threads), all exact:

| | bytes | packing | unpacking | processor time, unpacking | peak memory (pack / unpack) |
|---|---|---|---|---|---|
| mix 2 (before) | 100 545 180 | 33.0 s (109 ms a frame) | 29.9 s (99 ms a frame) | | |
| **mix 3 + the strike, 16 bands** | **101 751 797** | **20.9 s (69 ms)** | **9.7 s (32 ms)** | 97.7 s, the workers in a job 92 % of the time | 321 MB / 135 MB |
| mix 3 + the strike, `--bands 24` | 101 956 595 | 19.3 s (64 ms) | 9.3 s (31 ms) | in a job 100 % of the time | |
| x264 lossless (qp 0, veryslow) | 100 839 923 | 16.9 s | 0.95 s | 6.4 s | 710 MB / 186 MB |
| FFV1 (level 3, 16 slices, gop 300) | 124 691 885 | 1.7 s | 1.5 s | 13.6 s | 218 MB / 98 MB |
| AV1 lossless (libaom, cpu-used 2) | 124 942 885 | about 730 s | 5.4 s | 12.2 s | not measured / 124 MB |

Of the 20.9 s of packing, the strikes themselves take 13.7 s (45 ms a frame); the rest
is reading the movie, hashing and keeping. Bands and bytes (24 frames): 4 bands
8 146 381; 8 +0.17 %; 12 +0.32 %; 16 +0.41 %; 23 +0.77 %; 34 +1.16 %; 68 +2.4 %.

What this shows: the mix now plays at the movie's speed (31 frames a second), 3.1 times
faster than before, and packs 1.6 times faster; the strike keeps the 12 threads in a job 92 to 100 % of the time,
so what is left is the work each sample takes (one thread alone: about 200 ms a frame).
x264 unpacks the same clip with 6.4 s of processor time where the mix takes 97.7 s.

`fbottle trial <raw yuv420p> <width> <height> <frames> [bands]` packs and unpacks frames
in memory both ways (frame by frame, and by the strike) and times them.

### Vectors to a quarter of a pixel, a light map, two guessers (2026-11-15, last)

Three options for `--mix`, each a flag in the frame's head, each exact (every frame
checked by its hash in the player; `fbottle raw` gives the source byte for byte).

- **`--quarter`** (an idea of x264's; the code is this project's own): a block's vector
  points between the pixels of the frame before, to a quarter of a pixel. Light between
  the pixels is made by H.264's six-pixel filter (the one the blocks already had, in
  src/block.rs), colour by the four pixels around. The packer looks at the eight places
  half a pixel around the best whole-pixel place, then the eight a quarter around that.
- **`--relight`** (the light trick without loss): a light map for each frame, a tile of
  32 x 32 of light (16 x 16 of colour) at a time, each plane its own: an offset and a
  gain (of 64), told from the same tile of the frame before. It is laid only on the
  frame before as the guessers see it: before' = clamp((gain x before) >> 6) + offset);
  the misses still lead to the real samples. The packer puts a tile right only when
  that brings it nearer by an eighth of a step a sample. `--relight-offset`: offsets only.
- **`--two`**: only the two learning guessers are weighed for the guess.

guitar_10s.mp4 (1080p, 302 frames), 16 bands, a window of 32:

| | bytes | packing | unpacking | tiles put right |
|---|---|---|---|---|
| `--mix` | 101 276 733 | 20.6 s | 10.0 s | |
| `--mix --relight` | 101 331 624 | 19.7 s | 9.6 s | 3.0 % |
| **`--mix --quarter`** | **96 201 938** | **26.4 s** | **9.9 s** | |
| `--mix --quarter --relight` | 96 243 339 | 26.7 s | 10.0 s | 2.8 % |
| `--mix --quarter --relight --two` | 97 378 785 | 26.5 s | 10.0 s | 2.8 % |
| x264 lossless (qp 0, veryslow) | 100 839 923 | 16.9 s | 0.95 s | |
| FFV1 (level 3, 16 slices, gop 300) | 124 691 885 | 1.7 s | 1.5 s | |
| AV1 lossless (libaom, cpu-used 2) | 124 942 885 | about 730 s | 5.4 s | |

(`--mix` alone is 101 276 733 here and 101 751 797 in the table above: a colour block's
vector is now half the light's exactly, not rounded down to a whole pixel.)

fade_2s.mp4 (60 frames, the light fades): `--mix --quarter` 14 277 266 bytes; with
`--relight` 14 133 124 (38.3 % of tiles put right); with `--relight-offset` 14 081 171
(36.9 %).

What this shows: with `--quarter` the bottle is 4.6 % smaller than x264 lossless and
23.0 % smaller than AV1 lossless, and unpacks as fast as before; packing takes 28 % longer
(the search between the pixels). The light map does nothing for a clip whose light is
steady (the learning guessers follow the light themselves) and takes 1.0 to 1.4 % off a
fade. Weighing only two guessers costs 1.2 % and does not make unpacking faster here.

### Fixed weights for a stretch of film (`--golden`, 2026-11-15, night)

A second way for `--mix` to guess, made to unpack fast (the owner's design: the answer
is worked out once by the packer, the player learns nothing for the guess).

- **A stretch** is `--chunk N` frames (60); its first frame stands alone, and nothing
  goes on from the stretch before: not the frame, not what the bands learned.
- **The packer** first finds every vector of the stretch (the map), then adds up, over
  every fourth frame (and the one that stands alone), what the best weights are worked
  out from, and solves them: 13 weights for a frame that leans on the one before, 7 for
  one that stands alone, for each plane. They are kept in the head of the stretch's
  first frame (384 bytes).
- **The guess** is one weighed sum with those weights. No learning guessers, no
  weighing of guessers, no putting right by the surroundings, no GAP.
- **A miss's yes/nos** are each said by one learner (how the frame before and the median
  stand from the guess, how large the misses nearby were, how many neighbours stayed);
  no mixers.
- **The player's strike**: a frame that stands alone waits for nothing, so stretches are
  unpacked side by side; what the bands of each stretch in the window have learned is
  kept apart.

The weights are solved here, by least squares (14 equations in 14 letters). Nuome's
Golden Answer solves systems of up to three letters today; asking it needs a module in
Nuome for larger systems, which is not written.

guitar_10s.mp4 (1080p, 302 frames), 16 bands, a window of 32, all exact (every frame
checked by its hash; `fbottle raw` gives the source byte for byte):

| | bytes | packing | unpacking |
|---|---|---|---|
| `--mix --quarter` | 96 201 938 | 26.4 s | 9.9 s (32.8 ms a frame) |
| **`--mix --quarter --golden`** (stretches of 60) | **111 645 569** | **20.0 s** | **3.75 s (12.4 ms a frame)** |
| the same, `--chunk 30` | 111 681 392 | 20.1 s | 3.61 s |
| the same, `--chunk 120` | 111 429 479 | 19.7 s | 3.76 s |
| x264 lossless (qp 0, veryslow) | 100 839 923 | 16.9 s | 0.95 s |
| FFV1 (level 3, 16 slices, gop 300) | 124 691 885 | 1.7 s | 1.5 s |
| AV1 lossless (libaom, cpu-used 2) | 124 942 885 | about 730 s | 5.4 s |

Where the packing's 20.0 s go (stretches of 60): finding every vector 9.1 s, working out
the weights 1.1 s, the strikes (guessing and writing) 3.9 s, the rest (reading the
movie, hashing, keeping, gluing) about 6 s. Where the player's time goes (timed stage by
stage, the clock's own load included): the one learner 29 %, the frame before and the
median 20 %, the contexts 20 %, the range coder 19 %, the weighed sum 13 %.

What this shows: unpacking is 2.6 times faster than the mix that learns (80 frames a
second) and packing 1.3 times faster; the bottle is 16.1 % larger than that mix and
10.7 % larger than x264 lossless, and still 10.6 % smaller than AV1 lossless. Stretches
cost little: 30 frames instead of 120 is 0.2 %. Tried before building, on the mix that
learns (24 frames): fixed weights alone +5.1 %, one learner alone +5.7 %, both +10.0 %.

### A raw video compression trick: stacked bit sheets (`fbottle sheets`, 2026-11-16)

A trick for raw video, apart from the packers (the owner's idea; not part of any
packing mode). Every sample is split into its eight bits, sheet 7 (the top bit) down
to sheet 0, and each bit is written by a learning yes/no coder that sees only the same
bit of the frame before. No guesser, no vectors, no neighbours; the player sees the
same bit, so nothing else is kept.

guitar_10s.mp4 as raw yuv420p (1080p, 302 frames, 939 340 800 bytes), 16 bands, 12
threads, every sample came back:

| | bytes | of the raw | writing | reading |
|---|---|---|---|---|
| raw video | 939 340 800 | 100 % | | |
| **sheets, the bit of the frame before alone** | **385 143 386** | **41.0 % (2.4 times smaller)** | 7.9 s | 7.8 s |
| sheets, four bits seen (the frame before, left, above, the sheet above) | 241 138 540 | 25.7 % (3.9 times smaller) | 7.4 s | 7.6 s |
| `--mix --quarter` | 96 201 938 | 10.2 % | 26.4 s | 9.9 s |
| x264 lossless (qp 0, veryslow) | 100 839 923 | 10.7 % | 16.9 s | 0.95 s |
| AV1 lossless (libaom, cpu-used 2) | 124 942 885 | 13.3 % | about 730 s | 5.4 s |

Sheet by sheet, 7 to 0: how often a bit is what it was in the frame before (light /
colour) 98.6 / 99.7, 97.3 / 99.7, 95.0 / 99.4, 91.8 / 97.8, 87.2 / 96.7, 80.6 / 93.7,
72.3 / 86.6, 62.9 / 77.5 % (noise would be 50 %); bytes 8.5, 13.5, 22.6, 35.6, 49.3,
66.5, 86.6, 102.6 million. The top sheets nearly stand still and cost little; the
bottom three are near noise and hold two thirds of the bytes.

What was tried with it and left: on a packed bottle the sheets find nothing (every
sheet of the mix's bytes matches the frame before 50.0 % of the time; 13 bytes to win
of 96 million); on what the mix's guess missed by they give 130 648 150 bytes (124 186 304
with four bits seen); inside the mix, a miss written as eight sheets by the mix's own
learners who know the bits above: 101 152 027 bytes, packing 44.3 s, unpacking 25.1 s.
