/*
 * qwen3_worker — keep Qwen3-ASR loaded and decode many utterances.
 *
 * Protocol (line-oriented, UTF-8):
 *   stdin:  WAV /absolute/path.wav\n
 *           QUIT\n
 *   stdout: READY\n
 *           OK <transcript>\n
 *           ERR <message>\n
 *
 * Load once at start (~2s). Each short phrase then ~0.3–0.6s instead of ~2.5s+.
 *
 * Build:
 *   gcc -O2 -o qwen3_worker qwen3_worker.c -lsherpa-onnx-c-api -lonnxruntime -lm -lpthread
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "sherpa-onnx/c-api/c-api.h"

static double now_s(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

/* Replace CR/LF in transcript so one response stays one line. */
static void sanitize_line(char *s) {
  for (; *s; ++s) {
    if (*s == '\n' || *s == '\r') {
      *s = ' ';
    }
  }
}

static int decode_wav(const SherpaOnnxOfflineRecognizer *rec, const char *path,
                      char *out, size_t out_cap) {
  const SherpaOnnxWave *wave = SherpaOnnxReadWave(path);
  if (!wave) {
    snprintf(out, out_cap, "read wave failed: %s", path);
    return -1;
  }
  if (wave->num_samples <= 0) {
    SherpaOnnxFreeWave(wave);
    snprintf(out, out_cap, "empty wave: %s", path);
    return -1;
  }

  const SherpaOnnxOfflineStream *stream = SherpaOnnxCreateOfflineStream(rec);
  if (!stream) {
    SherpaOnnxFreeWave(wave);
    snprintf(out, out_cap, "create stream failed");
    return -1;
  }

  SherpaOnnxAcceptWaveformOffline(stream, wave->sample_rate, wave->samples,
                                  wave->num_samples);
  SherpaOnnxDecodeOfflineStream(rec, stream);

  const SherpaOnnxOfflineRecognizerResult *r =
      SherpaOnnxGetOfflineStreamResult(stream);
  const char *text = (r && r->text) ? r->text : "";
  snprintf(out, out_cap, "%s", text);
  sanitize_line(out);

  if (r) {
    SherpaOnnxDestroyOfflineRecognizerResult(r);
  }
  SherpaOnnxDestroyOfflineStream(stream);
  SherpaOnnxFreeWave(wave);
  return 0;
}

static void usage(const char *argv0) {
  fprintf(stderr,
          "Usage: %s --conv=... --encoder=... --decoder=... --tokenizer=... "
          "[--threads=N] [--max-new-tokens=N] [--hotwords=...]\n",
          argv0);
}

int main(int argc, char **argv) {
  const char *conv = NULL;
  const char *encoder = NULL;
  const char *decoder = NULL;
  const char *tokenizer = NULL;
  const char *hotwords = "";
  int threads = 6;
  int max_new_tokens = 128;

  for (int i = 1; i < argc; i++) {
    if (strncmp(argv[i], "--conv=", 7) == 0) {
      conv = argv[i] + 7;
    } else if (strncmp(argv[i], "--encoder=", 10) == 0) {
      encoder = argv[i] + 10;
    } else if (strncmp(argv[i], "--decoder=", 10) == 0) {
      decoder = argv[i] + 10;
    } else if (strncmp(argv[i], "--tokenizer=", 12) == 0) {
      tokenizer = argv[i] + 12;
    } else if (strncmp(argv[i], "--threads=", 10) == 0) {
      threads = atoi(argv[i] + 10);
    } else if (strncmp(argv[i], "--max-new-tokens=", 17) == 0) {
      max_new_tokens = atoi(argv[i] + 17);
    } else if (strncmp(argv[i], "--hotwords=", 11) == 0) {
      hotwords = argv[i] + 11;
    } else if (strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0) {
      usage(argv[0]);
      return 0;
    } else {
      fprintf(stderr, "unknown arg: %s\n", argv[i]);
      usage(argv[0]);
      return 1;
    }
  }

  if (!conv || !encoder || !decoder || !tokenizer) {
    usage(argv[0]);
    return 1;
  }
  if (threads < 1) {
    threads = 1;
  }
  if (max_new_tokens < 16) {
    max_new_tokens = 16;
  }

  /* Line-buffer stdout so Rust sees READY/OK immediately. */
  setvbuf(stdout, NULL, _IOLBF, 0);
  setvbuf(stderr, NULL, _IOLBF, 0);

  SherpaOnnxOfflineRecognizerConfig config;
  memset(&config, 0, sizeof(config));
  config.feat_config.sample_rate = 16000;
  config.feat_config.feature_dim = 80;
  config.decoding_method = "greedy_search";
  config.model_config.debug = 0;
  config.model_config.num_threads = threads;
  config.model_config.provider = "cpu";
  config.model_config.qwen3_asr.conv_frontend = conv;
  config.model_config.qwen3_asr.encoder = encoder;
  config.model_config.qwen3_asr.decoder = decoder;
  config.model_config.qwen3_asr.tokenizer = tokenizer;
  config.model_config.qwen3_asr.max_total_len = 512;
  config.model_config.qwen3_asr.max_new_tokens = max_new_tokens;
  config.model_config.qwen3_asr.temperature = 1e-6f;
  config.model_config.qwen3_asr.top_p = 0.8f;
  config.model_config.qwen3_asr.seed = 42;
  config.model_config.qwen3_asr.hotwords = hotwords;

  fprintf(stderr, "qwen3_worker: loading model (threads=%d)…\n", threads);
  double t0 = now_s();
  const SherpaOnnxOfflineRecognizer *rec =
      SherpaOnnxCreateOfflineRecognizer(&config);
  if (!rec) {
    fprintf(stderr, "qwen3_worker: CreateOfflineRecognizer failed\n");
    printf("ERR create recognizer failed\n");
    fflush(stdout);
    return 2;
  }
  fprintf(stderr, "qwen3_worker: ready in %.2fs\n", now_s() - t0);
  printf("READY\n");
  fflush(stdout);

  char line[8192];
  char msg[8192];
  while (fgets(line, sizeof(line), stdin)) {
    /* strip trailing newline */
    size_t n = strlen(line);
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
      line[--n] = '\0';
    }
    if (n == 0) {
      continue;
    }
    if (strcmp(line, "QUIT") == 0 || strcmp(line, "quit") == 0) {
      break;
    }
    if (strncmp(line, "WAV ", 4) == 0 || strncmp(line, "wav ", 4) == 0) {
      const char *path = line + 4;
      while (*path == ' ') {
        ++path;
      }
      double t1 = now_s();
      if (decode_wav(rec, path, msg, sizeof(msg)) == 0) {
        printf("OK %s\n", msg);
        fprintf(stderr, "qwen3_worker: decode %.3fs n=%zu\n", now_s() - t1,
                strlen(msg));
      } else {
        printf("ERR %s\n", msg);
        fprintf(stderr, "qwen3_worker: %s\n", msg);
      }
      fflush(stdout);
      continue;
    }
    if (strcmp(line, "PING") == 0) {
      printf("OK pong\n");
      fflush(stdout);
      continue;
    }
    printf("ERR unknown command\n");
    fflush(stdout);
  }

  SherpaOnnxDestroyOfflineRecognizer(rec);
  fprintf(stderr, "qwen3_worker: exit\n");
  return 0;
}
