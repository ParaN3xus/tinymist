//! Crityp is a crate for benchmarking typst code.
//!
//! ## Usage
//!
//! ```rs
//! let mut crit = criterion::Criterion::default();
//! let mut verse = tinymist_world::CompileOnceArgs::parse().resolve()?;
//! let mut world = verse.snapshot();
//! crityp::bench(&mut crit, &mut world)?;
//! crit.final_summary();
//! ```

use anyhow::Context as ContextTrait;
use comemo::Track;
use criterion::Criterion;
use ecow::{eco_format, EcoString};
use tinymist_world::reflexo_typst::path::unix_slash;
use tinymist_world::LspWorld;
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{Context, Func, Value};
use typst::introspection::Introspector;
use typst::World;

/// Runs benchmarks on the given world. An entry point must be provided in the
/// world.
pub fn bench(c: &mut Criterion, world: &mut LspWorld) -> anyhow::Result<()> {
    // Gets the main source file and its path.
    let main_source = world.source(world.main())?;
    let main_path = unix_slash(world.main().vpath().as_rooted_path());

    let route = Route::default();
    let mut sink = Sink::default();
    let traced = Traced::default();
    let introspector = Introspector::default();

    // Evaluates the main source file.
    let module = typst::eval::eval(
        ((world) as &dyn World).track(),
        traced.track(),
        sink.track_mut(),
        route.track(),
        &main_source,
    );
    let module = module
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("evaluation error")?;

    // Collects all benchmarks.
    let mut goals: Vec<(EcoString, &Func)> = vec![];
    for (name, value, _) in module.scope().iter() {
        if !name.starts_with("bench") {
            continue;
        }

        if let Value::Func(func) = value {
            goals.push((eco_format!("{main_path}@{name}"), func));
        }
    }

    // Runs benchmarks.
    for (name, func) in goals {
        let route = Route::default();
        let mut sink = Sink::default();
        let engine = &mut Engine {
            world: ((world) as &dyn World).track(),
            introspector: introspector.track(),
            traced: traced.track(),
            sink: sink.track_mut(),
            route,
        };

        // Runs the benchmark once.
        let mut call_once = move || {
            let context = Context::default();
            let values = Vec::<Value>::default();
            func.call(engine, context.track(), values)
        };

        // Calls the benchmark once to ensure it is correct.
        // Since all typst functions are pure, we can safely ignore the result
        // in the benchmark loop then.
        if let Err(err) = call_once() {
            eprintln!("call error in {name}: {err:?}");
            continue;
        }

        // Benchmarks the function
        c.bench_function(&name, move |b| {
            b.iter(|| {
                comemo::evict(0);
                let _result = call_once();
            })
        });
    }

    Ok(())
}