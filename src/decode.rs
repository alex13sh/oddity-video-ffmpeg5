extern crate ffmpeg_next as ffmpeg;

use ffmpeg::{
  codec::decoder::Video as AvDecoder,
  software::scaling::{
    context::Context as AvScaler,
    flag::Flags as AvScalerFlags,
  },
  util::{
    format::pixel::Pixel as AvPixel,
    error::EAGAIN,
  },
  Error as AvError,
  Rational as AvRational,
};

use super::{
  Error,
  Locator,
  RawFrame,
  io::Reader,
  options::Options,
  frame::FRAME_PIXEL_FORMAT,
  ffi::copy_frame_props,
};

#[cfg(feature = "ndarray")]
use super::{
  Frame,
  Time,
  ffi::convert_frame_to_ndarray_rgb24,
};

type Result<T> = std::result::Result<T, Error>;

/// Decodes video streams and provides the caller with decoded RGB frames.
/// 
/// # Example
/// 
/// ```
/// let decoder = Decoder::new(&PathBuf::from("video.mp4")).unwrap();
/// decoder
///   .decode_iter()
///   .take_while(Result::is_ok)
///   .for_each(|frame| println!("Got frame!"));
/// ```
pub struct Decoder {
  reader: Reader,
  reader_stream_index: usize,
  decoder: AvDecoder,
  decoder_time_base: AvRational,
  scaler: AvScaler,
  size: (u32, u32),
  frame_rate: f32,
}

impl Decoder {

  /// Create a new decoder for the specified file.
  /// 
  /// # Arguments
  /// 
  /// * `source` - Locator to file to decode.
  pub fn new(
    source: &Locator,
  ) -> Result<Self> {
    Self::from_reader(
      Reader::new(source)?,
      None,
    )
  }

  /// Create a new decoder for the specified file with input options.
  /// 
  /// # Arguments
  /// 
  /// * `source` - Locator to file to decode.
  /// * `options` - The input options.
  pub fn new_with_options(
    source: &Locator,
    options: &Options,
  ) -> Result<Self> {
    Self::from_reader(
      Reader::new_with_options(source, options)?,
      None,
    )
  }

  /// Create a new decoder for the specified file with input options and
  /// custom dimensions. Each frame will be resized to the given dimensions.
  /// 
  /// # Arguments
  /// 
  /// * `source` - Locator to file to decode.
  /// * `options` - The input options.
  /// * `resize` - How to resize frames.
  /// 
  /// # Example
  /// 
  /// ```
  /// let decoder = Decoder::new_with_options_and_resize(
  ///     &PathBuf::from("from_file.mp4").into(),
  ///     Options::new_with_rtsp_transport_tcp(),
  ///     Resize::Exact(800, 600))
  ///  .unwrap();
  /// ```
  pub fn new_with_options_and_resize(
    source: &Locator,
    options: &Options,
    resize: Resize,
  ) -> Result<Self> {
    Self::from_reader(
      Reader::new_with_options(source, options)?,
      Some(resize),
    )
  }

  /// Decode frames through iterator interface. This is similar to `decode`
  /// but it returns frames through an infinite iterator.
  /// 
  /// # Example
  /// 
  /// ```
  /// decoder
  ///   .decode_iter()
  ///   .take_while(Result::is_ok)
  ///   .map(Result::unwrap)
  ///   .for_each(|(ts, frame)| {
  ///     // Do something with frame...
  ///   });
  /// ```
  #[cfg(feature = "ndarray")]
  pub fn decode_iter(
    &mut self,
  ) -> impl Iterator<Item=Result<(Time, Frame)>> + '_ {
    std::iter::from_fn(move || {
      Some(self.decode())
    })
  }

  /// Decode a single frame.
  /// 
  /// # Returns
  /// 
  /// A tuple of the frame timestamp (relative to the stream) and the
  /// frame itself.
  /// 
  /// # Example
  /// 
  /// ```
  /// loop {
  ///   let (ts, frame) = decoder.decode()?;
  ///   // Do something with frame...
  /// }
  /// ```
  #[cfg(feature = "ndarray")]
  pub fn decode(&mut self) -> Result<(Time, Frame)> {
    let frame = &mut self.decode_raw()?;
    // We use the packet DTS here (which is `frame->pkt_dts`) because that is
    // what the encoder will use when encoding for the `PTS` field.
    let timestamp = Time::new(Some(frame.packet().dts), self.decoder_time_base);
    let frame = convert_frame_to_ndarray_rgb24(frame)
      .map_err(Error::BackendError)?;

    Ok((timestamp, frame))
  }

  /// Decode frames through iterator interface. This is similar to `decode_raw`
  /// but it returns frames through an infinite iterator.
  pub fn decode_raw_iter(
    &mut self,
  ) -> impl Iterator<Item=Result<RawFrame>> + '_ {
    std::iter::from_fn(move || {
      Some(self.decode_raw())
    })
  }

  /// Decode a single frame and return the raw ffmpeg `AvFrame`.
  pub fn decode_raw(&mut self) -> Result<RawFrame> {
    let mut frame: Option<RawFrame> = None;
    while frame.is_none() {
      let mut packet = self
        .reader
        .read(self.reader_stream_index)?
        .into_inner();
      packet.rescale_ts(self.stream_time_base(), self.decoder_time_base);

      self.decoder.send_packet(&packet)
        .map_err(Error::BackendError)?;

      frame = self.decoder_receive_frame()?;
    }

    let frame = frame.unwrap();
    let mut frame_scaled = RawFrame::empty();
    self
      .scaler
      .run(&frame, &mut frame_scaled)
      .map_err(Error::BackendError)?;

    copy_frame_props(&frame, &mut frame_scaled);

    Ok(frame_scaled)
  }

  /// Get the decoders input size (resolution dimensions): width and height.
  pub fn size(&self) -> (u32, u32) {
    self.size
  }

  /// Get the decoders input frame rate as floating-point value.
  pub fn frame_rate(&self) -> f32 {
    self.frame_rate
  }

  /// Create a decoder from a `Reader` instance. Optionally provide
  /// dimensions to resize frames to.
  /// 
  /// # Arguments
  /// 
  /// * `reader` - `Reader` to create decoder from.
  /// * `resize` - Optional resize strategy to apply to frames.
  fn from_reader(
    reader: Reader,
    resize: Option<Resize>,
  ) -> Result<Self> {
    let reader_stream_index = reader.best_video_stream_index()?;
    let reader_stream = reader
      .input
      .stream(reader_stream_index)
      .ok_or(AvError::StreamNotFound)?;

    let frame_rate = reader_stream.rate();
    let frame_rate = frame_rate.numerator() as f32 / frame_rate.denominator() as f32;
    
    let codec = reader_stream.codec().unwrap();
    let decoder = codec
      .decoder()
      .video()?;
    let decoder_time_base = decoder.time_base();

    let (resize_width, resize_height) = resize
      .map(|resize| match resize {
        Resize::Exact(w, h) => (w, h),
        Resize::Fit(w, h) => calculate_fit_dims(
          (decoder.width(), decoder.height()),
          (w, h)),
      })
      .unwrap_or((decoder.width(), decoder.height()));

    if decoder.format() == AvPixel::None ||
       decoder.width() == 0 || decoder.height() == 0 {
      return Err(Error::MissingCodecParameters);
    }

    let scaler = AvScaler::get(
      decoder.format(),
      decoder.width(),
      decoder.height(),
      FRAME_PIXEL_FORMAT,
      resize_width,
      resize_height,
      AvScalerFlags::AREA)?;

    let size = (decoder.width(), decoder.height());

    Ok(Self {
      reader,
      reader_stream_index,
      decoder,
      decoder_time_base,
      scaler,
      size,
      frame_rate,
    })
  }
  
  /// Pull a decoded frame from the decoder. This function also implements
  /// retry mechanism in case the decoder signals `EAGAIN`.
  fn decoder_receive_frame(&mut self) -> Result<Option<RawFrame>> {
    let mut frame = RawFrame::empty();
    let decode_result = self.decoder.receive_frame(&mut frame);
    match decode_result {
      Ok(())
        => Ok(Some(frame)),
      Err(AvError::Other { errno }) if errno == EAGAIN
        => Ok(None),
      Err(err)
        => Err(err.into()),
    }
  }

  // Acquire the time base of the input stream.
  fn stream_time_base(&self) -> AvRational {
    self
      .reader
      .input
      .stream(self.reader_stream_index)
      .unwrap()
      .time_base()
  }

}

impl Drop for Decoder {

  fn drop(&mut self) {
    // Maximum number of invocations to `decoder_receive_frame`
    // to drain the items still on the queue before giving up.
    const MAX_DRAIN_ITERATIONS: u32 = 100;

    // We need to drain the items still in the decoders queue.
    if let Ok(()) = self.decoder.send_eof() {
      for _ in 0..MAX_DRAIN_ITERATIONS {
        if self.decoder_receive_frame().is_err() {
          break;
        }
      }
    }
  }

}

/// Represents the possible resize strategies.
pub enum Resize {
  /// When resizing with `Resize::Exact`, each frame will be
  /// resized to the exact width and height given, without
  /// taking into account aspect ratio.
  Exact(u32, u32),
  /// When resizing with `Resize::Fit`, each frame will be
  /// resized to the biggest width and height possible within
  /// the given dimensions, without changing the aspect ratio.
  Fit(u32, u32),
}

/// Calculates the maximum image dimensions `w` and `h` that
/// fit inside `w_max` and `h_max` retaining the original
/// aspect ratio.
/// 
/// # Arguments
/// 
/// * `dims` - Original dimensions: width and height.
/// * `fit_dims` - Dimensions to fit in: width and height.
/// 
/// # Returns
/// 
/// The dimensions width and height.
fn calculate_fit_dims(
  dims: (u32, u32),
  fit_dims: (u32, u32),
) -> (u32, u32) {
  let (w, h) = dims;
  let (w_max, h_max) = fit_dims;
  if w_max >= w && h_max >= h {
    (w, h)
  } else {
    let wf = w_max as f32 / w as f32;
    let hf = h_max as f32 / h as f32;
    let f = wf.min(hf);
    ((w as f32 * f) as u32, 
      (h as f32 * f) as u32)
  }
}
