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

Build with `cargo build --release`. It needs `ffmpeg`, `ffprobe` and
`ffplay` on the path, Strike's sources next to this folder (`../Strike`),
the particle maker's vault (`~/Aurelia/vault`, or `AURELIA_VAULT`) and, for
the report's numbers, Nuome's `nuome` (`~/Nuome/target/release`, or `NUOME`).

## The seven steps

1. **CHOP** ([src/chop.rs](src/chop.rs)). ffmpeg gives every frame of the
   movie, none dropped and none doubled, in the movie's own pixel format
   (8- and 10-bit YUV, grey, RGB; anything else is taken as RGB or RGBA,
   and the report says so).
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
   Nimbus. A frame already in the window is struck from it. The frames go
   to ffplay at the playback rate.
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

## What it is not, yet

- No sound: a bottle holds the frames only.
- A frame rate that changes inside a movie is played at its average.
- The window keeps whole frames in Nimbus: about 95 frames at most, which
  is 1.2 GB for 4K. The packed shares of every bottle made also stay in
  Nimbus (`~/.strike/nimbus`) until they are removed there.
- The vectors are whole pixels per 16-pixel block, so a guessed frame can
  show block edges where things move fast or turn.
- It was not built with Strike itself; cargo built it.

The clips are not in the repository. The 4K one is two seconds of *Big Buck
Bunny* (c) Blender Foundation, CC BY 3.0, peach.blender.org.
