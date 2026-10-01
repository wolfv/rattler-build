# Generated sub-steps

A build step can *generate* steps at run time: commands it only knows after it
ran, such as one compile per source file once CMake has configured the
project. rattler-build runs them after the step, in parallel, and caches each
one by the content of its inputs. A rebuild, or another build of the same
sources, only runs the sub-steps whose inputs changed.

This complements the other layers: [`build.metadata`](metadata.md) generates
the authored steps *before* solving, and a [step cache](caching.md) skips a
whole step. Generated sub-steps are finer-grained, and their outputs are
restored from the cache rather than just verified.

## Ninja: one sub-step per edge

```yaml title="recipe.yaml"
build:
  steps:
    - name: configure
      run: cmake -G Ninja -S . -B build -DCMAKE_INSTALL_PREFIX=$PREFIX
    - name: compile
      depends_on: [configure]
      run: '"$RATTLER_BUILD_EXE" steps from-ninja build'
    - name: install
      depends_on: [compile]
      run: cmake --install build
```

`rattler-build steps from-ninja DIR [TARGETS...]` reads `DIR/build.ninja`
(CMake, Meson, GN, ...). It writes one sub-step for every edge needed to build
the targets, which default to the file's default targets:

- the command, with ninja's variables expanded;
- its inputs (explicit and implicit) and outputs;
- order-only prerequisites (generated headers), used for ordering only;
- the depfile and response file.

CMake's own regeneration edge is left out. The `install` step then runs against
the build directory as usual. See
[`examples/dynamic-steps`](https://github.com/prefix-dev/rattler-build/tree/main/examples/dynamic-steps).

## The steps file

Every step receives `RATTLER_BUILD_STEPS_FILE` (and `RATTLER_BUILD_EXE`, the
running rattler-build). A step that writes JSON there generates steps:

```json
{
  "cwd": "/path/to/work/build",
  "env": {"PATH": "...", "CC": "..."},
  "steps": [
    {
      "name": "CC util.o",
      "run": "cc -MD -MF util.o.d -c ../src/util.c -o util.o",
      "inputs": ["../src/util.c"],
      "outputs": ["util.o"],
      "after": ["version.h"],
      "depfile": "util.o.d"
    },
    {"name": "link", "run": "cc util.o -o app", "inputs": ["util.o"], "outputs": ["app"]}
  ]
}
```

- `cwd` and `env` apply to all steps. A generator should record its own
  environment, which is the activated build environment; `from-ninja` does.
  Without `env`, the steps get the build's base environment.
- Per step:
  - `name` and `run` are required.
  - `cwd` and `env` extend the file's.
  - `inputs`, `outputs`, `after`, `depends_on` (names), `depfile`,
    `rspfile` (`[path, content]`), and `cache` (default `true`).
- Relative paths are relative to the step's `cwd`.
- A step runs after the steps that produce its `inputs` or `after` files, and
  after its `depends_on`. They run in parallel up to the number of CPUs.
- All generated steps finish before the next authored step starts. A failing
  step fails the build with its command and output.

## Caching

A generated step's key covers:

- its command, working directory and environment;
- its response file;
- the content of its `inputs`.

The build's checkout-specific paths (`SRC_DIR`, `PREFIX`, `BUILD_PREFIX`,
`RECIPE_DIR`, the build directory) are replaced by placeholders, so keys are
the same across builds and checkouts. Headers named in the step's depfile are
recorded after it ran and checked on later lookups, the way ccache's manifests
work.

Outputs are stored content-addressed in `$RATTLER_BUILD_ACTION_CACHE` (default:
`<rattler cache>/rattler-build-actions`; `off` disables caching) and restored
on a hit. An unchanged object file therefore isn't relinked: after a header
edit that changes no code, the compiles rerun but the links stay cached.

!!! warning "Prototype"
    Output contents aren't relocated. A cached object file keeps the source
    paths of the build that produced it in its debug information. Commands
    run unsandboxed. The cache trusts its own directory.
