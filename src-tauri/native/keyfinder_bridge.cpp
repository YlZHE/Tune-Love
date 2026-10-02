#include <algorithm>
#include <cstring>
#include <exception>
#include <memory>
#include <cstdint>
#include <cmath>
#include "audiodata.h"
#include "keyfinder.h"
#include "toneprofiles.h"

namespace {
void write_error(char* output, size_t capacity, const char* message) noexcept {
  if (output == nullptr || capacity == 0) return;
  const char* safe = message == nullptr ? "unknown native keyfinder error" : message;
  const size_t length = std::min(capacity - 1, std::strlen(safe));
  std::memcpy(output, safe, length);
  output[length] = '\0';
}

// Keep upstream sources untouched. A second workspace finalizes a bounded copy
// of the tail for classification; zero-padding never enters the continuing stream.
struct Stream {
  KeyFinder::KeyFinder finder;
  KeyFinder::Workspace live;
  KeyFinder::Workspace preview;
  uint64_t frames = 0;
  uint64_t complete_hops = 0;
};

void clear_audio(KeyFinder::Workspace& workspace) {
  workspace.remainderBuffer = KeyFinder::AudioData();
  workspace.preprocessedBuffer = KeyFinder::AudioData();
  delete workspace.chromagram;
  workspace.chromagram = nullptr;
  delete workspace.lpfBuffer;
  workspace.lpfBuffer = nullptr;
  // fftAdapter remains owned by Workspace and can be reused after a reset.
}

// Optional lab evidence only. Reuse upstream profiles and similarity code,
// keeping the actual winner chosen by the original classifier.
void write_scores(const KeyFinder::Workspace& workspace, double* scores) {
  if (!scores) return;
  const auto chroma = workspace.chromagram->collapseToOneHop();
  KeyFinder::ToneProfile major(KeyFinder::toneProfileMajor());
  KeyFinder::ToneProfile minor(KeyFinder::toneProfileMinor());
  for (unsigned int i = 0; i < SEMITONES; ++i) {
    scores[i * 2] = major.cosineSimilarity(chroma, i);
    scores[i * 2 + 1] = minor.cosineSimilarity(chroma, i);
  }
}

// Twelve pitch-class magnitudes (C first; upstream band 0 is C1) averaged over
// hops [first, last) of a chromagram, summed across octaves. Scale-match evidence.
void write_chroma(const KeyFinder::Chromagram& chromagram, unsigned int first,
                  unsigned int last, double* chroma) {
  if (!chroma) return;
  for (unsigned int s = 0; s < SEMITONES; ++s) chroma[s] = 0.0;
  if (last <= first) return;
  for (unsigned int h = first; h < last; ++h)
    for (unsigned int b = 0; b < BANDS; ++b)
      chroma[b % SEMITONES] += chromagram.getMagnitude(h, b) / (last - first);
}
}

extern "C" void* keyfinder_stream_create(char* error, size_t capacity) noexcept {
  try { return new Stream(); }
  catch (const std::exception& e) { write_error(error, capacity, e.what()); }
  catch (...) { write_error(error, capacity, "cannot create keyfinder stream"); }
  return nullptr;
}

extern "C" void keyfinder_stream_destroy(void* context) noexcept {
  delete static_cast<Stream*>(context);
}

// stats: [0] retained hops, [1] buffered samples, [2] complete hops added by this
// feed, [3] input frames per hop. chroma: optional 12 doubles averaged over
// exactly those new hops (zero when none).
extern "C" int keyfinder_stream_feed(
    void* context, const double* samples, size_t count, int reset, double gain,
    int* key, unsigned int* stats, double* scores, double* chroma, char* error, size_t capacity) noexcept {
  try {
    if (!context || !samples || !key || !stats || count == 0 || count > 384000
        || !std::isfinite(gain) || gain < 1.0 || gain > 64.0) {
      write_error(error, capacity, "invalid keyfinder stream argument");
      return -1;
    }
    auto& stream = *static_cast<Stream*>(context);
    if (reset) {
      clear_audio(stream.live);
      clear_audio(stream.preview);
      stream.frames = 0;
      stream.complete_hops = 0;
    }
    KeyFinder::AudioData audio;
    audio.setChannels(1);
    audio.setFrameRate(48000);
    audio.addToSampleCount(static_cast<unsigned int>(count));
    for (size_t i = 0; i < count; ++i) audio.setSample(static_cast<unsigned int>(i), samples[i]);
    auto& live = stream.live;
    const unsigned int before = live.chromagram ? live.chromagram->getHops() : 0;
    stream.finder.progressiveChromagram(audio, live);
    stream.frames += count;
    const unsigned int added = live.chromagram->getHops() - before;
    stream.complete_hops += added;
    write_chroma(*live.chromagram, before, live.chromagram->getHops(), chroma);
    stats[2] = added;

    // Retain only full FFT hops beginning within the latest eight seconds.
    // Hop alignment can omit less than one hop at the left edge; unlike the
    // old batch path the FFT grid is anchored to the capture epoch.
    const auto factor = static_cast<uint64_t>(std::floor(48000.0 / 2 / (KeyFinder::getLastFrequency() * 1.10)));
    const uint64_t hop_frames = HOPSIZE * factor;
    const uint64_t start_frame = stream.frames > 384000 ? stream.frames - 384000 : 0;
    const uint64_t first_wanted = (start_frame + hop_frames - 1) / hop_frames;
    const uint64_t first_retained = stream.complete_hops - live.chromagram->getHops();
    const auto discard = static_cast<unsigned int>(std::min<uint64_t>(
        live.chromagram->getHops(), first_wanted > first_retained ? first_wanted - first_retained : 0));
    if (discard) {
      auto trimmed = std::make_unique<KeyFinder::Chromagram>(live.chromagram->getHops() - discard);
      for (unsigned int h = 0; h < trimmed->getHops(); ++h)
        for (unsigned int b = 0; b < BANDS; ++b)
          trimmed->setMagnitude(h, b, live.chromagram->getMagnitude(h + discard, b));
      delete live.chromagram;
      live.chromagram = trimmed.release();
    }

    auto& preview = stream.preview;
    clear_audio(preview);
    preview.preprocessedBuffer = live.preprocessedBuffer;
    preview.preprocessedBuffer.resetIterators();
    preview.chromagram = new KeyFinder::Chromagram(*live.chromagram);
    if (preview.preprocessedBuffer.getSampleCount() > 0)
      stream.finder.finalChromagram(preview);
    for (unsigned int h = 0; h < preview.chromagram->getHops(); ++h)
      for (unsigned int b = 0; b < BANDS; ++b)
        preview.chromagram->setMagnitude(h, b, preview.chromagram->getMagnitude(h, b) * gain);
    *key = static_cast<int>(stream.finder.keyOfChromagram(preview));
    write_scores(preview, scores);
    stats[0] = preview.chromagram->getHops();
    stats[1] = live.preprocessedBuffer.getSampleCount() + live.remainderBuffer.getSampleCount();
    stats[3] = static_cast<unsigned int>(hop_frames);
    return 0;
  } catch (const std::exception& e) { write_error(error, capacity, e.what()); }
  catch (...) { write_error(error, capacity, "unknown keyfinder stream exception"); }
  return -1;
}

// chroma: optional 12 doubles averaged over the whole window; hops receives
// [0] hop count and [1] input frames per hop.
extern "C" int keyfinder_detect_mono(
    const double* samples,
    size_t sample_count,
    unsigned int sample_rate,
    int* output_key,
    double* scores,
    double* chroma,
    unsigned int* hops,
    char* error,
    size_t error_capacity) noexcept {
  try {
    if (samples == nullptr || output_key == nullptr) {
      write_error(error, error_capacity, "null keyfinder bridge argument");
      return -1;
    }
    KeyFinder::AudioData audio;
    audio.setChannels(1);
    audio.setFrameRate(sample_rate);
    audio.addToSampleCount(static_cast<unsigned int>(sample_count));
    for (size_t index = 0; index < sample_count; ++index) {
      audio.setSample(static_cast<unsigned int>(index), samples[index]);
    }
    KeyFinder::KeyFinder finder;
    if (scores || chroma || hops) {
      // Same steps as upstream keyOfAudio, keeping the workspace for evidence.
      KeyFinder::Workspace workspace;
      finder.progressiveChromagram(audio, workspace);
      finder.finalChromagram(workspace);
      *output_key = static_cast<int>(finder.keyOfChromagram(workspace));
      write_scores(workspace, scores);
      write_chroma(*workspace.chromagram, 0, workspace.chromagram->getHops(), chroma);
      if (hops) {
        hops[0] = workspace.chromagram->getHops();
        const auto factor = static_cast<unsigned int>(
            std::floor(sample_rate / 2.0 / (KeyFinder::getLastFrequency() * 1.10)));
        hops[1] = HOPSIZE * factor;
      }
    } else {
      *output_key = static_cast<int>(finder.keyOfAudio(audio));
    }
    return 0;
  } catch (const std::exception& exception) {
    write_error(error, error_capacity, exception.what());
    return -1;
  } catch (...) {
    write_error(error, error_capacity, "unknown native keyfinder exception");
    return -1;
  }
}
