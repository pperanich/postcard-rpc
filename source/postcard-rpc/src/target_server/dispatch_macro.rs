/// # Define Dispatch Macro
///
/// ```rust
/// # use postcard_rpc::target_server::dispatch_macro::fake::*;
/// # use postcard_rpc::{endpoint, target_server::{sender::Sender, SpawnContext}, WireHeader, define_dispatch};
/// # use postcard::experimental::schema::Schema;
/// # use embassy_usb_driver::{Bus, ControlPipe, EndpointIn, EndpointOut};
/// # use serde::{Deserialize, Serialize};
///
/// pub struct DispatchCtx;
/// pub struct SpawnCtx;
///
/// // This trait impl is necessary if you want to use the `spawn` variant,
/// // as spawned tasks must take ownership of any context they need.
/// impl SpawnContext for DispatchCtx {
///     type SpawnCtxt = SpawnCtx;
///     fn spawn_ctxt(&mut self) -> Self::SpawnCtxt {
///         SpawnCtx
///     }
/// }
///
/// define_dispatch! {
///     dispatcher: Dispatcher<
///         Mutex = FakeMutex,
///         Driver = FakeDriver,
///         Context = DispatchCtx,
///     >;
///     AlphaEndpoint => async alpha_handler,
///     BetaEndpoint => async beta_handler,
///     GammaEndpoint => async gamma_handler,
///     DeltaEndpoint => blocking delta_handler,
///     EpsilonEndpoint => spawn epsilon_handler_task,
/// }
///
/// async fn alpha_handler(_c: &mut DispatchCtx, _h: WireHeader, _b: AReq) -> AResp {
///     todo!()
/// }
///
/// async fn beta_handler(_c: &mut DispatchCtx, _h: WireHeader, _b: BReq) -> BResp {
///     todo!()
/// }
///
/// async fn gamma_handler(_c: &mut DispatchCtx, _h: WireHeader, _b: GReq) -> GResp {
///     todo!()
/// }
///
/// fn delta_handler(_c: &mut DispatchCtx, _h: WireHeader, _b: DReq) -> DResp {
///     todo!()
/// }
///
/// #[embassy_executor::task]
/// async fn epsilon_handler_task(_c: SpawnCtx, _h: WireHeader, _b: EReq, _sender: Sender<FakeMutex, FakeDriver>) {
///     todo!()
/// }
/// ```

#[macro_export]
macro_rules! define_dispatch {
    // This is the "blocking execution" arm for defining an endpoint
    (@arm blocking ($endpoint:ty) $handler:ident $context:ident $header:ident $req:ident $dispatch:ident) => {
        {
            let reply = $handler($context, $header.clone(), $req);
            if $dispatch.sender.reply::<$endpoint>($header.seq_no, &reply).await.is_err() {
                let err = $crate::standard_icd::WireError::SerFailed;
                $dispatch.error($header.seq_no, err).await;
            }
        }
    };
    // This is the "async execution" arm for defining an endpoint
    (@arm async ($endpoint:ty) $handler:ident $context:ident $header:ident $req:ident $dispatch:ident) => {
        {
            let reply = $handler($context, $header.clone(), $req).await;
            if $dispatch.sender.reply::<$endpoint>($header.seq_no, &reply).await.is_err() {
                let err = $crate::standard_icd::WireError::SerFailed;
                $dispatch.error($header.seq_no, err).await;
            }
        }
    };
    // This is the "spawn an embassy task" arm for defining an endpoint
    (@arm spawn ($endpoint:ty) $handler:ident $context:ident $header:ident $req:ident $dispatch:ident) => {
        {
            let spawner = ::embassy_executor::Spawner::for_current_executor().await;
            let context = $crate::target_server::SpawnContext::spawn_ctxt($context);
            if spawner.spawn($handler(context, $header.clone(), $req, $dispatch.sender())).is_err() {
                let err = $crate::standard_icd::WireError::FailedToSpawn;
                $dispatch.error($header.seq_no, err).await;
            }
        }
    };
    // Optional trailing comma lol
    (
        dispatcher: $name:ident<Mutex = $mutex:ty, Driver = $driver:ty, Context = $context:ty,>;
        $($endpoint:ty => $flavor:tt $handler:ident,)*
    ) => {
        define_dispatch! {
            dispatcher: $name<Mutex = $mutex, Driver = $driver, Context = $context>;
            $(
                $endpoint => $flavor $handler,
            )*
        }
    };
    // This is the main entrypoint
    (
        dispatcher: $name:ident<Mutex = $mutex:ty, Driver = $driver:ty, Context = $context:ty>;
        $($endpoint:ty => $flavor:tt $handler:ident,)*
    ) => {
        /// This is a structure that handles dispatching, generated by the
        /// `postcard-rpc::define_dispatch!()` macro.
        pub struct $name {
            pub sender: $crate::target_server::sender::Sender<$mutex, $driver>,
            pub context: $context,
        }

        impl $name {
            /// Create a new instance of the dispatcher
            pub fn new(
                tx_buf: &'static mut [u8],
                ep_in: <$driver as ::embassy_usb::driver::Driver<'static>>::EndpointIn,
                context: $context,
            ) -> Self {
                static SENDER_INNER: ::static_cell::StaticCell<
                    ::embassy_sync::mutex::Mutex<$mutex, $crate::target_server::sender::SenderInner<$driver>>,
                > = ::static_cell::StaticCell::new();
                $name {
                    sender: $crate::target_server::sender::Sender::init_sender(&SENDER_INNER, tx_buf, ep_in),
                    context,
                }
            }
        }

        impl $crate::target_server::Dispatch for $name {
            type Mutex = $mutex;
            type Driver = $driver;

            async fn dispatch(
                &mut self,
                hdr: $crate::WireHeader,
                body: &[u8],
            ) {
                // Unreachable patterns lets us know if we had any duplicated request keys.
                // If you hit this error: you either defined the same endpoint twice, OR you've
                // had a schema collision.
                #[deny(unreachable_patterns)]
                match hdr.key {
                    $(
                        <$endpoint as $crate::Endpoint>::REQ_KEY => {
                            // Can we deserialize the request?
                            let Ok(req) = postcard::from_bytes::<<$endpoint as $crate::Endpoint>::Request>(body) else {
                                let err = $crate::standard_icd::WireError::DeserFailed;
                                self.error(hdr.seq_no, err).await;
                                return;
                            };

                            // Store some items as named bindings, so we can use `ident` in the
                            // recursive macro expansion. Load bearing order: we borrow `context`
                            // from `dispatch` because we need `dispatch` AFTER `context`, so NLL
                            // allows this to still borrowck
                            let dispatch = self;
                            let context = &mut dispatch.context;

                            // This will expand to the right "flavor" of handler
                            define_dispatch!(@arm $flavor ($endpoint) $handler context hdr req dispatch);
                        }
                    )*
                    other => {
                        // huh! We have no idea what this key is supposed to be!
                        let err = $crate::standard_icd::WireError::UnknownKey(other.to_bytes());
                        self.error(hdr.seq_no, err).await;
                        return;
                    },
                }
            }

            async fn error(
                &self,
                seq_no: u32,
                error: $crate::standard_icd::WireError,
            ) {
                // If we get an error while sending an error, welp there's not much we can do
                let _ = self.sender.reply_keyed(seq_no, $crate::standard_icd::ERROR_KEY, &error).await;
            }

            fn sender(&self) -> $crate::target_server::sender::Sender<Self::Mutex, Self::Driver> {
                self.sender.clone()
            }
        }

    }
}

/// This is a basic example that everything compiles. It is intended to exercise the macro above,
/// as well as provide impls for docs. Don't rely on any of this!
#[doc(hidden)]
#[allow(dead_code)]
pub mod fake {
    use crate::target_server::SpawnContext;
    #[allow(unused_imports)]
    use crate::{endpoint, target_server::sender::Sender, Schema, WireHeader};
    use embassy_usb_driver::{Bus, ControlPipe, EndpointIn, EndpointOut};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Schema)]
    pub struct AReq;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct AResp;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct BReq;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct BResp;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct GReq;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct GResp;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct DReq;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct DResp;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct EReq;
    #[derive(Serialize, Deserialize, Schema)]
    pub struct EResp;

    endpoint!(AlphaEndpoint, AReq, AResp, "alpha");
    endpoint!(BetaEndpoint, BReq, BResp, "beta");
    endpoint!(GammaEndpoint, GReq, GResp, "gamma");
    endpoint!(DeltaEndpoint, DReq, DResp, "delta");
    endpoint!(EpsilonEndpoint, EReq, EResp, "epsilon");

    pub struct FakeMutex;
    pub struct FakeDriver;
    pub struct FakeEpOut;
    pub struct FakeEpIn;
    pub struct FakeCtlPipe;
    pub struct FakeBus;

    impl embassy_usb_driver::Endpoint for FakeEpOut {
        fn info(&self) -> &embassy_usb_driver::EndpointInfo {
            todo!()
        }

        async fn wait_enabled(&mut self) {
            todo!()
        }
    }

    impl EndpointOut for FakeEpOut {
        async fn read(
            &mut self,
            _buf: &mut [u8],
        ) -> Result<usize, embassy_usb_driver::EndpointError> {
            todo!()
        }
    }

    impl embassy_usb_driver::Endpoint for FakeEpIn {
        fn info(&self) -> &embassy_usb_driver::EndpointInfo {
            todo!()
        }

        async fn wait_enabled(&mut self) {
            todo!()
        }
    }

    impl EndpointIn for FakeEpIn {
        async fn write(&mut self, _buf: &[u8]) -> Result<(), embassy_usb_driver::EndpointError> {
            todo!()
        }
    }

    impl ControlPipe for FakeCtlPipe {
        fn max_packet_size(&self) -> usize {
            todo!()
        }

        async fn setup(&mut self) -> [u8; 8] {
            todo!()
        }

        async fn data_out(
            &mut self,
            _buf: &mut [u8],
            _first: bool,
            _last: bool,
        ) -> Result<usize, embassy_usb_driver::EndpointError> {
            todo!()
        }

        async fn data_in(
            &mut self,
            _data: &[u8],
            _first: bool,
            _last: bool,
        ) -> Result<(), embassy_usb_driver::EndpointError> {
            todo!()
        }

        async fn accept(&mut self) {
            todo!()
        }

        async fn reject(&mut self) {
            todo!()
        }

        async fn accept_set_address(&mut self, _addr: u8) {
            todo!()
        }
    }

    impl Bus for FakeBus {
        async fn enable(&mut self) {
            todo!()
        }

        async fn disable(&mut self) {
            todo!()
        }

        async fn poll(&mut self) -> embassy_usb_driver::Event {
            todo!()
        }

        fn endpoint_set_enabled(
            &mut self,
            _ep_addr: embassy_usb_driver::EndpointAddress,
            _enabled: bool,
        ) {
            todo!()
        }

        fn endpoint_set_stalled(
            &mut self,
            _ep_addr: embassy_usb_driver::EndpointAddress,
            _stalled: bool,
        ) {
            todo!()
        }

        fn endpoint_is_stalled(&mut self, _ep_addr: embassy_usb_driver::EndpointAddress) -> bool {
            todo!()
        }

        async fn remote_wakeup(&mut self) -> Result<(), embassy_usb_driver::Unsupported> {
            todo!()
        }
    }

    impl embassy_usb_driver::Driver<'static> for FakeDriver {
        type EndpointOut = FakeEpOut;

        type EndpointIn = FakeEpIn;

        type ControlPipe = FakeCtlPipe;

        type Bus = FakeBus;

        fn alloc_endpoint_out(
            &mut self,
            _ep_type: embassy_usb_driver::EndpointType,
            _max_packet_size: u16,
            _interval_ms: u8,
        ) -> Result<Self::EndpointOut, embassy_usb_driver::EndpointAllocError> {
            todo!()
        }

        fn alloc_endpoint_in(
            &mut self,
            _ep_type: embassy_usb_driver::EndpointType,
            _max_packet_size: u16,
            _interval_ms: u8,
        ) -> Result<Self::EndpointIn, embassy_usb_driver::EndpointAllocError> {
            todo!()
        }

        fn start(self, _control_max_packet_size: u16) -> (Self::Bus, Self::ControlPipe) {
            todo!()
        }
    }

    unsafe impl embassy_sync::blocking_mutex::raw::RawMutex for FakeMutex {
        const INIT: Self = Self;

        fn lock<R>(&self, _f: impl FnOnce() -> R) -> R {
            todo!()
        }
    }

    pub struct TestContext {
        pub a: u32,
        pub b: u32,
    }

    impl SpawnContext for TestContext {
        type SpawnCtxt = TestSpawnContext;

        fn spawn_ctxt(&mut self) -> Self::SpawnCtxt {
            TestSpawnContext { b: self.b }
        }
    }

    pub struct TestSpawnContext {
        b: u32,
    }

    define_dispatch! {
        dispatcher: TestDispatcher<Mutex = FakeMutex, Driver = FakeDriver, Context = TestContext>;
        AlphaEndpoint => async test_alpha_handler,
        BetaEndpoint => async test_beta_handler,
        GammaEndpoint => async test_gamma_handler,
        DeltaEndpoint => blocking test_delta_handler,
        EpsilonEndpoint => spawn test_epsilon_handler_task,
    }

    async fn test_alpha_handler(
        _context: &mut TestContext,
        _header: WireHeader,
        _body: AReq,
    ) -> AResp {
        todo!()
    }

    async fn test_beta_handler(
        _context: &mut TestContext,
        _header: WireHeader,
        _body: BReq,
    ) -> BResp {
        todo!()
    }

    async fn test_gamma_handler(
        _context: &mut TestContext,
        _header: WireHeader,
        _body: GReq,
    ) -> GResp {
        todo!()
    }

    fn test_delta_handler(_context: &mut TestContext, _header: WireHeader, _body: DReq) -> DResp {
        todo!()
    }

    #[embassy_executor::task]
    async fn test_epsilon_handler_task(
        _context: TestSpawnContext,
        _header: WireHeader,
        _body: EReq,
        _sender: Sender<FakeMutex, FakeDriver>,
    ) {
        todo!()
    }
}