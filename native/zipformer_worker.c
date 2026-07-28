/*
 * zipformer_worker — warm streaming Zipformer (partials for dual-model IME).
 *
 * Line protocol (UTF-8):
 *   stdin:
 *     START\n
 *     PCM <nbytes>\n
 *     <raw s16le mono bytes, exactly nbytes>
 *     FINISH\n
 *     RESET\n
 *     QUIT\n
 *   stdout:
 *     READY\n
 *     PARTIAL <text>\n   after PCM (may be empty)
 *     FINAL <text>\n     after FINISH
 *     OK\n               after START/RESET
 *     ERR <msg>\n
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "sherpa-onnx/c-api/c-api.h"

static void sanitize(char *s) {
  for (; s && *s; ++s) {
    if (*s == '\n' || *s == '\r') {
      *s = ' ';
    }
  }
}

static void emit_partial(const SherpaOnnxOnlineRecognizer *rec,
                         const SherpaOnnxOnlineStream *stream,
                         int is_final) {
  const SherpaOnnxOnlineRecognizerResult *r =
      SherpaOnnxGetOnlineStreamResult(rec, stream);
  const char *text = (r && r->text) ? r->text : "";
  char buf[8192];
  snprintf(buf, sizeof(buf), "%s", text);
  sanitize(buf);
  if (is_final) {
    printf("FINAL %s\n", buf);
  } else {
    printf("PARTIAL %s\n", buf);
  }
  fflush(stdout);
  if (r) {
    SherpaOnnxDestroyOnlineRecognizerResult(r);
  }
}

static void feed_s16(const SherpaOnnxOnlineRecognizer *rec,
                     const SherpaOnnxOnlineStream *stream, int32_t sample_rate,
                     const int16_t *pcm, int32_t n) {
  if (n <= 0) {
    return;
  }
  float *samples = (float *)malloc((size_t)n * sizeof(float));
  if (!samples) {
    return;
  }
  for (int32_t i = 0; i < n; i++) {
    samples[i] = (float)pcm[i] / 32768.0f;
  }
  SherpaOnnxOnlineStreamAcceptWaveform(stream, sample_rate, samples, n);
  free(samples);

  while (SherpaOnnxIsOnlineStreamReady(rec, stream)) {
    SherpaOnnxDecodeOnlineStream(rec, stream);
  }
}

static void usage(const char *argv0) {
  fprintf(stderr,
          "Usage: %s --encoder= --decoder= --joiner= --tokens= "
          "[--threads=N] [--sample-rate=16000]\n",
          argv0);
}

int main(int argc, char **argv) {
  const char *encoder = NULL;
  const char *decoder = NULL;
  const char *joiner = NULL;
  const char *tokens = NULL;
  int threads = 2;
  int sample_rate = 16000;

  for (int i = 1; i < argc; i++) {
    if (strncmp(argv[i], "--encoder=", 10) == 0) {
      encoder = argv[i] + 10;
    } else if (strncmp(argv[i], "--decoder=", 10) == 0) {
      decoder = argv[i] + 10;
    } else if (strncmp(argv[i], "--joiner=", 9) == 0) {
      joiner = argv[i] + 9;
    } else if (strncmp(argv[i], "--tokens=", 9) == 0) {
      tokens = argv[i] + 9;
    } else if (strncmp(argv[i], "--threads=", 10) == 0) {
      threads = atoi(argv[i] + 10);
    } else if (strncmp(argv[i], "--sample-rate=", 14) == 0) {
      sample_rate = atoi(argv[i] + 14);
    } else if (strcmp(argv[i], "-h") == 0 || strcmp(argv[i], "--help") == 0) {
      usage(argv[0]);
      return 0;
    }
  }

  if (!encoder || !decoder || !joiner || !tokens) {
    usage(argv[0]);
    return 1;
  }
  if (threads < 1) {
    threads = 1;
  }

  setvbuf(stdout, NULL, _IOLBF, 0);
  setvbuf(stderr, NULL, _IOLBF, 0);

  SherpaOnnxOnlineRecognizerConfig config;
  memset(&config, 0, sizeof(config));
  config.feat_config.sample_rate = sample_rate;
  config.feat_config.feature_dim = 80;
  config.model_config.tokens = tokens;
  config.model_config.num_threads = threads;
  config.model_config.provider = "cpu";
  config.model_config.debug = 0;
  config.model_config.transducer.encoder = encoder;
  config.model_config.transducer.decoder = decoder;
  config.model_config.transducer.joiner = joiner;
  config.decoding_method = "greedy_search";
  config.max_active_paths = 4;
  config.enable_endpoint = 1;
  config.rule1_min_trailing_silence = 2.4f;
  config.rule2_min_trailing_silence = 1.2f;
  config.rule3_min_utterance_length = 20.0f;

  fprintf(stderr, "zipformer_worker: loading…\n");
  const SherpaOnnxOnlineRecognizer *rec =
      SherpaOnnxCreateOnlineRecognizer(&config);
  if (!rec) {
    printf("ERR create online recognizer failed\n");
    return 2;
  }
  fprintf(stderr, "zipformer_worker: READY\n");
  printf("READY\n");
  fflush(stdout);

  const SherpaOnnxOnlineStream *stream = NULL;
  char line[4096];
  while (fgets(line, sizeof(line), stdin)) {
    size_t n = strlen(line);
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
      line[--n] = '\0';
    }
    if (n == 0) {
      continue;
    }
    if (strcmp(line, "QUIT") == 0) {
      break;
    }
    if (strcmp(line, "START") == 0 || strcmp(line, "RESET") == 0) {
      if (stream) {
        SherpaOnnxDestroyOnlineStream(stream);
        stream = NULL;
      }
      stream = SherpaOnnxCreateOnlineStream(rec);
      if (!stream) {
        printf("ERR create stream failed\n");
        fflush(stdout);
        continue;
      }
      printf("OK\n");
      fflush(stdout);
      continue;
    }
    if (strcmp(line, "FINISH") == 0) {
      if (!stream) {
        printf("FINAL \n");
        fflush(stdout);
        continue;
      }
      SherpaOnnxOnlineStreamInputFinished(stream);
      while (SherpaOnnxIsOnlineStreamReady(rec, stream)) {
        SherpaOnnxDecodeOnlineStream(rec, stream);
      }
      emit_partial(rec, stream, 1);
      SherpaOnnxDestroyOnlineStream(stream);
      stream = SherpaOnnxCreateOnlineStream(rec);
      continue;
    }
    if (strncmp(line, "PCM ", 4) == 0) {
      long nbytes = strtol(line + 4, NULL, 10);
      /* Always consume the declared binary payload so the protocol stays aligned. */
      int bad = (nbytes <= 0 || nbytes > 16 * 1024 * 1024 || (nbytes % 2) != 0);

      if (bad) {
        if (nbytes > 0 && nbytes <= 16 * 1024 * 1024) {
          /* Discard declared payload even when size is odd/invalid. */
          char discard[4096];
          long left = nbytes;
          while (left > 0) {
            size_t want = (size_t)(left > (long)sizeof(discard) ? sizeof(discard) : left);
            size_t r = fread(discard, 1, want, stdin);
            if (r == 0) {
              break;
            }
            left -= (long)r;
          }
        }
        printf("ERR bad PCM size\n");
        fflush(stdout);
        continue;
      }
      if (!stream) {
        stream = SherpaOnnxCreateOnlineStream(rec);
      }
      unsigned char *buf = (unsigned char *)malloc((size_t)nbytes);
      if (!buf) {
        /* Still drain stdin payload. */
        char discard[4096];
        long left = nbytes;
        while (left > 0) {
          size_t want = (size_t)(left > (long)sizeof(discard) ? sizeof(discard) : left);
          size_t r = fread(discard, 1, want, stdin);
          if (r == 0) {
            break;
          }
          left -= (long)r;
        }
        printf("ERR oom\n");
        fflush(stdout);
        continue;
      }
      size_t got = 0;
      while (got < (size_t)nbytes) {
        size_t r = fread(buf + got, 1, (size_t)nbytes - got, stdin);
        if (r == 0) {
          break;
        }
        got += r;
      }
      if (got != (size_t)nbytes) {
        free(buf);
        printf("ERR short PCM read\n");
        fflush(stdout);
        continue;
      }
      int32_t nsamp = (int32_t)(nbytes / 2);
      feed_s16(rec, stream, sample_rate, (const int16_t *)buf, nsamp);
      free(buf);

      /* Dual-model: VAD owns phrase cuts. Do not auto-reset on Zipformer endpoint
       * (would desync from Qwen3 finalize). Just report partial text. */
      emit_partial(rec, stream, 0);
      continue;
    }
    printf("ERR unknown command\n");
    fflush(stdout);
  }

  if (stream) {
    SherpaOnnxDestroyOnlineStream(stream);
  }
  SherpaOnnxDestroyOnlineRecognizer(rec);
  return 0;
}
