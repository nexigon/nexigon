//! Data structures for decoding and encoding of frames.
//!
//! Internally, frames are always represented as [`Bytes`] to avoid excessive copying, in
//! particular, when frames are provided and sent over a Websocket. Each frame has a tag
//! indicating its type and optionally multiple fields. All fields of a frame, except the
//! last field, are required to have a fixed size. The last field can
//! be a dynamically-sized byte sequence.

use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use thiserror::Error;

use super::ChannelId;

/// Macro for unwrapping [`Option`] in `const` contexts.
macro_rules! const_option_unwrap {
    ($expr:expr) => {
        match $expr {
            Some(value) => value,
            None => panic!("expected some value, found `None`"),
        }
    };
}

/// Macro for defining all frame types.
///
/// We define this macro not because it is easier to read than implementing everything
/// manually, but because it prevents subtile bugs (e.g., slightly wrong offsets) that may
/// result from implementing everything manually.
macro_rules! define_frame_types {
    ($(
        $(#[$meta:meta])*
        $name:ident($tag:literal) { $($fields:tt)* }
    )*) => {
        paste::paste! {
            /// A frame.
            #[derive(Debug, Clone)]
            pub(super) enum Frame<B = Bytes> {
                $(
                    $(#[$meta])*
                    $name([<Frame $name>]<B>),
                )*
            }

            impl<B: AsRef<[u8]>> Frame<B> {
                /// Byte representation of the frame.
                pub fn as_bytes(&self) -> &[u8] {
                    match self {
                        $(
                            Self::$name(frame) => frame.bytes.as_ref(),
                        )*
                    }
                }

                /// Parse a frame.
                pub fn parse(bytes: B) -> Result<Self, InvalidFrameError> {
                    let bytes_slice = bytes.as_ref();
                    if bytes_slice.is_empty() {
                        return Err(InvalidFrameError::InvalidLength(1))
                    }
                    match bytes_slice[0] {
                        $(
                            $tag => {
                                if bytes_slice.len() < <[<Frame $name>]<B>>::MIN_FRAME_SIZE {
                                    Err(InvalidFrameError::InvalidLength(<[<Frame $name>]<B>>::MIN_FRAME_SIZE))
                                } else {
                                    Ok(Self::$name([<Frame $name>]::<B> {
                                        bytes
                                    }))
                                }
                            },
                        )*
                        _ => {
                            Err(InvalidFrameError::InvalidTag(bytes_slice[0]))
                        }
                    }
                }
            }

            impl From<Frame<Bytes>> for Bytes {
                fn from(frame: Frame<Bytes>) -> Self {
                    match frame {
                        $(
                            Frame::$name(frame) => frame.bytes
                        ),*
                    }
                }
            }

            $(
                define_frame_types!(@frame $(#[$meta])* $name($tag) [ $($fields)* ]);

                #[allow(dead_code)]
                impl<B> [<Frame $name>]<B> {
                    /// Tag used to identify the respective frames.
                    pub const FRAME_TAG: u8 = $tag;

                    /// Construct a frame from raw bytes without any checks.
                    pub fn from_raw_bytes(bytes: B) -> Self {
                        Self { bytes }
                    }

                    define_frame_types!(@offsets (Some(1), Some(0)) [ $($fields)* ]);
                }

                impl From<[<Frame $name>]<Bytes>> for Frame<Bytes> {
                    fn from(frame: [<Frame $name>]<Bytes>) -> Self {
                        Self::$name(frame)
                    }
                }

                impl From<[<Frame $name>]<BytesMut>> for Frame<Bytes> {
                    fn from(frame: [<Frame $name>]<BytesMut>) -> Self {
                        Self::$name(frame.into())
                    }
                }

                impl From<[<Frame $name>]<BytesMut>> for [<Frame $name>]<Bytes> {
                    fn from(frame: [<Frame $name>]<BytesMut>) -> Self {
                        Self { bytes: frame.bytes.freeze() }
                    }
                }

                #[allow(dead_code)]
                impl [<Frame $name>]<Bytes> {
                    define_frame_types!(@new [ $($fields)* ]);
                    define_frame_types!(@setters [ $($fields)* ]);
                }

                #[allow(dead_code)]
                impl [<Frame $name>]<BytesMut> {
                    define_frame_types!(@setters [ $($fields)* ]);
                }

                #[allow(dead_code)]
                impl<B: AsRef<[u8]>> [<Frame $name>]<B> {
                    /// Length of the frame.
                    pub fn len(&self) -> usize {
                        self.bytes.as_ref().len()
                    }

                    define_frame_types!(@getters [ $($fields)* ]);
                }
            )*
        }
    };

    // Macro for generating the constants for the field offsets.
    (@offsets ($offset:expr, $previous_offset:expr) [
        $(#[$field_meta:meta])*
        $field_name:ident : $field_type:ty
        $(,$($tail:tt)*)?
    ]) => {
        paste::paste! {
            #[doc = "Offset of the `" $field_name "` field."]
            pub const [<FIELD_ $field_name:upper _OFFSET>]: usize = const_option_unwrap!($offset);
        }
        define_frame_types! {
            @offsets (
                add_optional_sizes($offset, <$field_type as FieldType>::FIELD_SIZE),
                $offset
            ) [
                $($($tail)*)*
            ]
        }
    };
    (@offsets ($offset:expr, $previous_offset:expr) []) => {
        /// Size of the frame and `None` for dynamically-sized frames.
        pub const FRAME_SIZE: Option<usize> = $offset;

        /// Minimal size of the frame.
        pub const MIN_FRAME_SIZE: usize = match Self::FRAME_SIZE {
            Some(size) => size,
            None => const_option_unwrap!($previous_offset),
        };
    };

    // Macro for generating field getters.
    (@getters  [
        $(
            $(#[$field_meta:meta])*
            $field_name:ident : $field_type:ty,
        )*
    ]) => {
        paste::paste! {
            $(
                $(#[$field_meta])*
                pub fn $field_name(&self) -> <$field_type as FieldType>::Decoded<'_> {
                    let offset = Self::[<FIELD_ $field_name:upper _OFFSET>];
                    match <$field_type as FieldType>::FIELD_SIZE {
                        Some(fixed) => {
                            <$field_type as FieldType>::decode(
                                &self.bytes.as_ref()[offset..offset + fixed]
                            )
                        }
                        None => {
                            <$field_type as FieldType>::decode(
                                &self.bytes.as_ref()[offset..]
                            )
                        }
                    }
                }
            )*
        }
    };

    // Macro for generating field setters.
    (@setters  [
        $(
            $(#[$field_meta:meta])*
            $field_name:ident : $field_type:ty,
        )*
    ]) => {
        paste::paste! {
            $(
                #[doc = "Setter for the `" $field_name "` field."]
                pub fn [<set_ $field_name>](&mut self, value: $field_type) {
                    let offset = Self::[<FIELD_ $field_name:upper _OFFSET>];
                    let mut bytes = BytesMut::from(std::mem::take(&mut self.bytes));
                    match <$field_type as FieldType>::FIELD_SIZE {
                        Some(fixed) => {
                            bytes.as_mut()[offset..offset + fixed].copy_from_slice(
                                value.encode().as_ref()
                            )
                        }
                        None => {
                            // This is guaranteed to be the last field. So, we just
                            // truncate the `Vec` and then encode the value into it.
                            bytes.truncate(offset);
                            value.encode_into_buffer(&mut bytes);
                        }
                    }
                    self.bytes = bytes.into();
                }
            )*
        }
    };

    // Macro for generating frame types.
    (@frame $(#[$meta:meta])* $name:ident($tag:literal) [
        $(
            $(#[$field_meta:meta])*
            $field_name:ident : $field_type:ty,
        )*
    ]) => {
        paste::paste! {
            $(#[$meta])*
            #[derive(Debug, Clone)]
            pub(super) struct [<Frame $name>]<B = Bytes> {
                pub(super) bytes: B
            }
        }
    };

    // Macro for generating constructors.
    (@new [
        $(
            $(#[$field_meta:meta])*
            $field_name:ident : $field_type:ty,
        )*
    ]) => {
        /// Create a new frame of the respective type.
        pub fn new($($field_name: $field_type),*) -> Self {
            #[allow(unused_mut)]
            let mut size = 1;
            $(
                size += $field_name.value_size();
            )*
            #[allow(unused_mut)]
            let mut bytes = BytesMut::with_capacity(size);
            bytes.put_u8(Self::FRAME_TAG);
            $(
                $field_name.encode_into_buffer(&mut bytes);
            )*
            Self { bytes: bytes.freeze() }
        }
    };
}

/// Protocol magic used to identify the protocol.
pub const PROTOCOL_MAGIC: [u8; 16] = [
    27, 27, 46, 134, 44, 205, 244, 157, 82, 109, 227, 13, 167, 171, 225, 140,
];

define_frame_types! {
    /// Initialize the connection.
    Hello(0x00) {
        /// Magic protocol identifier.
        magic: &[u8; 16],
        /// Information about the sender of the frame.
        info: &[u8],
    }
    /// Close the connection.
    Close(0xFF) {
        /// Reason why the connection should be closed.
        reason: &[u8],
    }
    /// Request to open a new channel.
    ChannelRequest(0x10){
        /// Channel at the sender of the frame.
        sender_id: ChannelId,
        /// Initial flow control credit for frames.
        frame_credit: u32,
        /// Initial flow control credit for bytes.
        byte_credit: u32,
        /// Endpoint of the request.
        endpoint: &[u8],
    }
    /// Accept a request to open a new channel.
    ChannelAccept(0x11){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// Channel id at the sender of the frame.
        sender_id: ChannelId,
        /// Initial flow control credit for frames.
        frame_credit: u32,
        /// Initial flow control credit for bytes.
        byte_credit: u32,
    }
    /// Reject a request to open a new channel.
    ChannelReject(0x12){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// The reason why the request has been rejected.
        reason: &[u8],
    }
    /// Channel data.
    ChannelData(0x13){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// Payload of the frame.
        payload: &[u8],
    }
    /// Adjust the flow control credit of a channel.
    ChannelAdjust(0x14){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// Flow control credit to add for frames.
        frame_credit: u32,
        /// Flow control credit to add for bytes.
        byte_credit: u32,
    }
    /// Close a channel.
    ChannelClose(0x15){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// The reason why the channel should be closed.
        reason: &[u8],
    }
    /// Indicate that the sending end of a channel has been closed.
    ChannelClosed(0x17){
        /// Channel id at the receiver of the frame.
        receiver_id: ChannelId,
        /// The reason why the channel has been closed.
        reason: &[u8],
    }
    /// Ping used for measuring the round-trip time.
    Ping(0x20) {}
    /// Pong used to measure the round-trip time.
    Pong(0x21) {}
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for Frame<B> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Error parsing a frame.
#[derive(Debug, Clone, Error)]
pub enum InvalidFrameError {
    /// The frame has an invalid tag.
    #[error("{0:#04x} is not a valid frame tag")]
    InvalidTag(u8),
    /// The frame has an invalid length.
    #[error("frame has an invalid length, expected at least {0} bytes")]
    InvalidLength(usize),
}

/// Abstraction for frame fields.
pub(super) trait FieldType: Sized {
    /// Size of the field and `None` for dynamically-sized fields.
    const FIELD_SIZE: Option<usize>;

    type Decoded<'b>;
    type Encoded<'f>: AsRef<[u8]>
    where
        Self: 'f;

    /// Size of a value of the type.
    fn value_size(&self) -> usize;

    /// Encode the value into a vector.
    fn encode_into_buffer<B: BufMut>(&self, buffer: &mut B);

    /// Decode a value.
    fn decode<'b>(bytes: &'b [u8]) -> Self::Decoded<'b>;

    /// Encode a value.
    fn encode<'f>(&'f self) -> Self::Encoded<'f>;
}

impl FieldType for ChannelId {
    const FIELD_SIZE: Option<usize> = Some(ChannelId::SIZE);

    type Decoded<'b> = Self;
    type Encoded<'f> = [u8; ChannelId::SIZE];

    fn value_size(&self) -> usize {
        const_option_unwrap!(Self::FIELD_SIZE)
    }

    fn encode_into_buffer<B: BufMut>(&self, buffer: &mut B) {
        buffer.put_slice(&self.to_bytes())
    }

    fn decode<'b>(bytes: &'b [u8]) -> Self::Decoded<'b> {
        ChannelId::from_bytes(bytes.try_into().expect("size should be correct"))
    }

    fn encode<'f>(&'f self) -> Self::Encoded<'f> {
        self.to_bytes()
    }
}

impl FieldType for u32 {
    const FIELD_SIZE: Option<usize> = Some(4);

    type Decoded<'b> = Self;
    type Encoded<'f> = [u8; 4];

    fn value_size(&self) -> usize {
        const_option_unwrap!(Self::FIELD_SIZE)
    }

    fn encode_into_buffer<B: BufMut>(&self, buffer: &mut B) {
        buffer.put_u32(*self)
    }

    fn decode<'b>(bytes: &'b [u8]) -> Self::Decoded<'b> {
        u32::from_be_bytes(bytes.try_into().expect("size should be correct"))
    }

    fn encode<'f>(&'f self) -> Self::Encoded<'f> {
        self.to_be_bytes()
    }
}

impl<const N: usize> FieldType for &[u8; N] {
    const FIELD_SIZE: Option<usize> = Some(N);

    type Decoded<'b> = &'b [u8; N];
    type Encoded<'f>
        = &'f [u8; N]
    where
        Self: 'f;

    fn value_size(&self) -> usize {
        self.len()
    }

    fn encode_into_buffer<B: BufMut>(&self, buffer: &mut B) {
        buffer.put_slice(*self)
    }

    fn decode<'b>(bytes: &'b [u8]) -> Self::Decoded<'b> {
        bytes.try_into().expect("size should be correct")
    }

    fn encode<'f>(&'f self) -> Self::Encoded<'f> {
        self
    }
}

impl FieldType for &[u8] {
    const FIELD_SIZE: Option<usize> = None;

    type Decoded<'b> = &'b [u8];
    type Encoded<'f>
        = &'f [u8]
    where
        Self: 'f;

    fn value_size(&self) -> usize {
        self.len()
    }

    fn encode_into_buffer<B: BufMut>(&self, buffer: &mut B) {
        buffer.put_slice(self)
    }

    fn decode<'b>(bytes: &'b [u8]) -> Self::Decoded<'b> {
        bytes
    }

    fn encode<'f>(&'f self) -> Self::Encoded<'f> {
        self
    }
}

/// Constant helper function for adding optional sizes.
const fn add_optional_sizes(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        _ => None,
    }
}
