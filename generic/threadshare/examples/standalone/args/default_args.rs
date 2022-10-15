use super::super::CAT;

#[derive(Copy, Clone, Debug)]
pub struct SyncMutexSink;

impl SyncMutexSink {
    pub fn element_name(self) -> &'static str {
        super::super::sink::SYNC_MUTEX_ELEMENT_NAME
    }
}

#[derive(Debug)]
pub struct Args {
    pub streams: u32,
    pub groups: u32,
    pub wait: u32,
    pub push_period: u32,
    pub num_buffers: i32,
    pub sink: SyncMutexSink,
    pub disable_stats_log: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            streams: 5000,
            groups: 2,
            wait: 20,
            push_period: 20,
            num_buffers: 5000,
            sink: SyncMutexSink,
            disable_stats_log: false,
        }
    }
}

pub fn args() -> Args {
    if std::env::args().len() > 1 {
        gst::warning!(CAT, "Ignoring command line arguments");
        gst::warning!(CAT, "Build with `--features=clap`");
    }

    let args = Args::default();
    gst::warning!(CAT, "{:?}", args);

    args
}
