# Writing background shaders

driftwm renders the canvas background using a GLSL fragment shader. You can
write your own to replace the default dot grid.

## How it works

The shader runs once per pixel every frame the viewport changes. It receives
the pixel's position and the viewport's camera offset, and outputs a color.
The result covers the entire output behind all windows.

## Inputs

### Built-in (provided by smithay)

| Name       | Type    | Description                                       |
| ---------- | ------- | ------------------------------------------------- |
| `v_coords` | `vec2`  | Normalized position within the output, 0.0–1.0    |
| `size`     | `vec2`  | Output dimensions in pixels (e.g. 1920.0, 1080.0) |
| `alpha`    | `float` | Element opacity, normally 1.0                     |

### Custom (provided by driftwm)

| Name       | Type    | Description                               |
| ---------- | ------- | ----------------------------------------- |
| `u_camera` | `vec2`  | Viewport position on the canvas in pixels |
| `u_time`   | `float` | Seconds since compositor start            |

`v_coords * size` gives screen-local pixel coordinates (top-left = 0,0).
Adding `u_camera` converts to canvas coordinates — this is how the background
scrolls with the viewport.

## Output

Set `gl_FragColor` to an RGBA `vec4`. Multiply by `alpha` to respect
compositor opacity:

```glsl
gl_FragColor = vec4(color, 1.0) * alpha;
```

## Minimal example

Solid color that shifts hue based on viewport position:

```glsl
precision mediump float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera;

void main() {
    vec2 canvas = (v_coords * size + u_camera) * 0.001;
    vec3 col = vec3(
        sin(canvas.x) * 0.5 + 0.5,
        sin(canvas.y) * 0.5 + 0.5,
        0.5
    );
    gl_FragColor = vec4(col, 1.0) * alpha;
}
```

## Tips

- **GLSL ES 1.0**: smithay auto-prepends `#version 100`. Don't add your own
  version directive. Use `precision mediump float;` or `highp` for noise.
- **Canvas coords**: The standard pattern is
  `vec2 canvas = (v_coords * size + u_camera) * scale;` where `scale`
  controls the feature size (smaller = larger features).
- **Float precision**: `u_camera` can be large (thousands of pixels from
  origin). If your shader uses `mod()` or `fract()` on canvas coords,
  reduce first: `mod(u_camera, period)` instead of `mod(canvas, period)`.
  See `dot_grid.glsl` for an example. Noise-based shaders using
  `floor()`/`fract()` internally are naturally resilient since the hash
  functions wrap.
- **Animated shaders**: `u_time` gives seconds since compositor start, enabling
  time-driven animations. driftwm re-renders every frame when a shader uses
  `u_time` — declare it in your shader and it will animate continuously.
- **Colors as constants**: Define colors, spacing, and other tunables as
  GLSL `const` values at the top of your shader. This keeps everything in
  one file — no config round-trip needed.
- **Shipped examples**: See `extras/wallpapers/` for dot grid, compass grid,
  noise clouds, dark sea, blue drift, and animated squares.

## Configuring the background

`[background]` accepts a `type` and a `path`. Three types are supported:

```toml
# Procedural GLSL shader — scrolls with the canvas
[background]
type = "shader"
path = "~/shaders/my_bg.glsl"

# Image tiled across the canvas (scrolls with the camera)
[background]
type = "tile"
path = "~/Pictures/tile.png"

# Single image fixed to the viewport (does not scroll or zoom).
# Cheapest mode: zero per-frame uniform updates, so blur and overlays
# above stay cached across pans.
[background]
type = "wallpaper"
path = "~/Pictures/wallpaper.png"
```

The `wallpaper` mode stretches the image to fill the output. Pick an image
sized to your monitor for best results.

### Legacy fields

`shader_path` and `tile_path` are still accepted for backwards compatibility
and behave like `type = "shader"` and `type = "tile"` respectively. They log
an info-level deprecation hint at startup; prefer `type` + `path` in new
configs.

If both `type` and a legacy field are set, `type` wins.

## Reloading after edits

The config is automatically reloaded when the file changes. The shader is
re-read from disk on every reload, so touch the config to pick up shader
edits:

```bash
touch ~/.config/driftwm/config.toml
```

To bind this to a key, add to your config:

```toml
[keybindings]
"mod+shift+c" = "spawn touch ~/.config/driftwm/config.toml"
```
