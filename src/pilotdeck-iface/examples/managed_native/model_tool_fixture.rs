//! Controlled fixture role exercising the real production Native Loop
//! composition (`ToolBatchCodec` + `InitialReadCodec` + `ModelSessionCodec`
//! under `ResourceMux` + `ProductController` events + `ProductionTurnClock`).
//! Delegates directly to [`pilotdeck_iface::scoped_native_entry::serve_scoped_model_tool_loop`].

pub fn serve() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build multi-thread runtime for scoped native model/tool loop")
        .block_on(
            pilotdeck_iface::scoped_native_entry::serve_scoped_model_tool_loop(
                "pilotdeck",
                "controller",
            ),
        )
        .expect("native model tool loop serving");
}
