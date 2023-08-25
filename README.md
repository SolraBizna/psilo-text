`psilo-text` is a Rust crate which facilitates rendering fonts at runtime, in realtime, using multichannel signed distance fields. It is part of the Psilo engine, but doesn't depend on any other part of the engine.

Signed distance fields are a clever way to get decent-quality realtime text rendering with low runtime cost. Multichannel signed distance fields, as originated by Viktor Chlumský, are an even more clever way to greatly improve the quality of the rendering with only a trivial increase in rendering overhead and even a potential *reduction* in video memory overhead. For more information, see [Chlumský's Master's thesis][1].

`psilo-text` is not the only crate you'll need to render text. At a minimum, you'll also need a shaping engine. I recommend [`rustybuzz`][2]. Information on how to use `psilo-text` can be found in [the crate documentation][3]. Unfortunately, there's not much here by way of examples yet.

[1]: https://github.com/Chlumsky/msdfgen/files/3050967/thesis.pdf
[2]: https://crates.io/crates/rustybuzz
[3]: https://docs.rs/psilo-text/latest/psilo-text/

# Legalese

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
