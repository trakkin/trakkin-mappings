package dev.trakkin.ingestion;

import static org.junit.jupiter.api.Assertions.*;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.ByteArrayOutputStream;
import java.nio.file.Path;
import java.util.*;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

class BridgeTest {
  @TempDir Path directory;

  private static ObjectNode request(String action, String warehouse) {
    return Mirror.JSON
        .createObjectNode()
        .put("version", 1)
        .put("action", action)
        .put("warehouse", warehouse);
  }

  private static JsonNode respond(Bridge bridge, JsonNode request) throws Exception {
    var output = new ByteArrayOutputStream();
    bridge.handle(request, output);
    var response = Mirror.JSON.readTree(output.toByteArray());
    assertEquals(1, response.path("version").asInt());
    assertTrue(response.path("ok").asBoolean());
    return response;
  }

  @Test
  void oneSessionKeepsWarehousesAndCheckpointsIsolated() throws Exception {
    String first = directory.resolve("first").toUri().toString();
    String second = directory.resolve("second").toUri().toString();
    try (var bridge = new Bridge(Map.of())) {
      for (String warehouse : List.of(first, second)) {
        respond(bridge, request("open", warehouse).put("create", true));
        String payload =
            Mirror.canonical(
                Mirror.JSON.createObjectNode().put("id", 1).put("warehouse", warehouse));
        var update =
            new Mirror.Update(
                new Mirror.Content(payload, Mirror.hash(payload), false),
                Mirror.JSON.createObjectNode());
        var commit = request("commit", warehouse).put("operation", "sync");
        commit.set("checkpoint", Mirror.JSON.createObjectNode().put("source", warehouse));
        commit.set("updates", Mirror.JSON.valueToTree(Map.of("1", update)));
        respond(bridge, commit);
      }
      for (String warehouse : List.of(first, second)) {
        respond(bridge, request("open", warehouse).put("create", false));
        var inspected =
            respond(bridge, request("inspect", warehouse).put("key", "1").put("limit", 1));
        assertEquals(
            warehouse, inspected.path("records").get(0).path("payload").path("warehouse").asText());
        var checkpoint = respond(bridge, request("checkpoint", warehouse).put("operation", "sync"));
        assertEquals(warehouse, checkpoint.path("checkpoint").path("source").asText());
      }
    }
  }

  @Test
  void failedAndUnopenedWarehousesDoNotBecomeRegistered() throws Exception {
    String warehouse = directory.toUri().toString();
    try (var bridge = new Bridge(Map.of())) {
      assertThrows(
          IllegalArgumentException.class, () -> respond(bridge, request("index", warehouse)));
      assertThrows(
          Exception.class, () -> respond(bridge, request("open", warehouse).put("create", false)));
      assertFalse(java.nio.file.Files.exists(directory.resolve("mirrors.db")));
      respond(bridge, request("open", warehouse).put("create", true));
      assertTrue(respond(bridge, request("index", warehouse)).path("entries").isEmpty());
    }
  }

  @Test
  void rejectsInvalidEnvelopeBeforeOpeningStorage() throws Exception {
    String warehouse = directory.toUri().toString();
    try (var bridge = new Bridge(Map.of())) {
      for (var invalid :
          List.of(
              request("open", warehouse).put("version", 2).put("create", true),
              request("open", warehouse).put("version", 4294967297L).put("create", true),
              request("open", warehouse).put("version", "1").put("create", true),
              request("open", warehouse).put("warehouse", "").put("create", true),
              request("open", warehouse).put("create", "true"))) {
        assertThrows(IllegalArgumentException.class, () -> respond(bridge, invalid));
      }
      assertFalse(java.nio.file.Files.exists(directory.resolve("mirrors.db")));
    }
  }

  @Test
  void responsesPreserveProtocolAndKeepOutputOpen() throws Exception {
    var output =
        new ByteArrayOutputStream() {
          @Override
          public void close() {
            fail("Response must not close the protocol output");
          }
        };
    var metadata = Mirror.JSON.readTree("{\"parent\":\"tv:1\",\"title\":\"caf\u00e9\\nfixture\"}");
    var entries = Map.of("100", new Mirror.Entry("hash", false, metadata));
    Bridge.respond(new HashMap<>(Map.of("entries", entries)), output);
    Bridge.respond(new HashMap<>(Collections.singletonMap("snapshot_id", null)), output);
    String text = output.toString(java.nio.charset.StandardCharsets.UTF_8);
    assertTrue(text.endsWith("\n"));
    var lines = text.lines().toList();
    assertEquals(2, lines.size());
    var response = Mirror.JSON.readTree(lines.getFirst());
    assertEquals(1, response.path("version").asInt());
    assertTrue(response.path("ok").asBoolean());
    assertEquals(Mirror.JSON.valueToTree(entries), response.path("entries"));
    assertTrue(Mirror.JSON.readTree(lines.get(1)).path("snapshot_id").isNull());
  }

  @Test
  void streamsLargeResponsesInBoundedWrites() throws Exception {
    int count = 32768;
    var metadata = Mirror.JSON.createObjectNode().put("fixture", "x".repeat(4096));
    var entry = new Mirror.Entry("hash", false, metadata);
    Map<String, Mirror.Entry> entries =
        new AbstractMap<>() {
          @Override
          public Set<Map.Entry<String, Mirror.Entry>> entrySet() {
            return new AbstractSet<>() {
              @Override
              public int size() {
                return count;
              }

              @Override
              public Iterator<Map.Entry<String, Mirror.Entry>> iterator() {
                return java.util.stream.IntStream.range(0, count)
                    .mapToObj(index -> Map.entry(Integer.toString(index), entry))
                    .iterator();
              }
            };
          }
        };
    var output =
        new java.io.OutputStream() {
          long bytes;
          int lastByte;

          @Override
          public void write(int value) {
            bytes++;
            lastByte = value;
          }

          @Override
          public void write(byte[] buffer, int offset, int length) {
            assertTrue(length <= 65536, "Response writes must be bounded");
            bytes += length;
            if (length > 0) lastByte = buffer[offset + length - 1];
          }

          @Override
          public void close() {
            fail("Response must not close the protocol output");
          }
        };
    Bridge.respond(new HashMap<>(Map.of("entries", entries)), output);
    assertTrue(output.bytes > 128L * 1024 * 1024);
    assertEquals('\n', output.lastByte);
  }

  @Test
  void mapsS3EnvironmentToHadoopEndpointRegion() {
    assertEquals(
        Map.of(
            "s3.endpoint",
            "https://example.r2.cloudflarestorage.com",
            "s3.endpoint.region",
            "auto",
            "s3.path.style.access",
            "true",
            "s3.access-key",
            "test-access",
            "s3.secret-key",
            "test-secret"),
        Bridge.storageSettings(
            Map.of(
                "TRAKKIN_MAPPINGS_INGESTION_S3_ENDPOINT",
                "https://example.r2.cloudflarestorage.com",
                "TRAKKIN_MAPPINGS_INGESTION_S3_REGION",
                "auto",
                "TRAKKIN_MAPPINGS_INGESTION_S3_PATH_STYLE_ACCESS",
                "true",
                "TRAKKIN_MAPPINGS_INGESTION_S3_ACCESS_KEY_ID",
                "test-access",
                "TRAKKIN_MAPPINGS_INGESTION_S3_SECRET_ACCESS_KEY",
                "test-secret")));
    assertEquals(
        Map.of(), Bridge.storageSettings(Map.of("TRAKKIN_MAPPINGS_INGESTION_S3_REGION", "")));
  }
}
