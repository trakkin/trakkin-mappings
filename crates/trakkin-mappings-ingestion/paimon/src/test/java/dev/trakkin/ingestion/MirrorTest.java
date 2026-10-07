package dev.trakkin.ingestion;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.file.Path;
import java.util.*;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

class MirrorTest {
  @TempDir Path directory;

  @Test
  void existingOpenDoesNotCreateMissingTable() {
    assertThrows(Exception.class, () -> new Mirror(directory.toUri().toString(), Map.of(), false));
    assertFalse(java.nio.file.Files.exists(directory.resolve("mirrors.db")));
  }

  @Test
  void existingOpenReadsCreatedTable() throws Exception {
    try (var writer = new Mirror(directory.toUri().toString(), Map.of())) {
      writer.commit(
          "bootstrap", Mirror.JSON.createObjectNode(), Map.of("1", prepared("{\"id\":1}")));
      try (var reader = new Mirror(directory.toUri().toString(), Map.of(), false)) {
        assertTrue(reader.index().containsKey("1"));
      }
    }
  }

  private static Mirror.Update prepared(String payload) throws Exception {
    return prepared(payload, false, Mirror.JSON.createObjectNode());
  }

  private static Mirror.Update prepared(
      String payload, boolean deleted, com.fasterxml.jackson.databind.JsonNode metadata)
      throws Exception {
    String canonical = Mirror.canonical(Mirror.JSON.readTree(payload));
    return new Mirror.Update(
        new Mirror.Content(canonical, Mirror.hash(canonical), deleted), metadata);
  }

  @Test
  void commitsDoNotReadUnrelatedCatalogueEntries() throws Exception {
    try (var mirror = new Mirror(directory.toUri().toString(), Map.of())) {
      var builder = mirror.table.newBatchWriteBuilder();
      try (var write = builder.newWrite();
          var commit = builder.newCommit()) {
        write.write(Mirror.row("@metadata/unrelated", "", "{", false));
        commit.commit(write.prepareCommit());
      }
      assertThrows(IllegalStateException.class, mirror::index);

      var checkpoint = Mirror.JSON.createObjectNode().put("watermark", 100);
      var metadata = Mirror.JSON.createObjectNode().put("parent", "tv:1");
      var update = prepared("{\"id\":100}", false, metadata);
      mirror.commit("sync", checkpoint, Map.of("100", update));
      mirror.commit("sync", checkpoint, Map.of("100", update));

      var replacement = Mirror.JSON.createObjectNode().put("parent", "tv:2");
      mirror.commit("sync", checkpoint, Map.of("100", new Mirror.Update(null, replacement)));
      assertEquals(replacement, mirror.inspect("100", 1, false).get(0).get("metadata"));
      mirror.commit(
          "sync",
          checkpoint,
          Map.of("100", prepared("{\"reason\":\"not_found\"}", true, replacement)));
      assertTrue(mirror.inspect("100", 1, false).get(0).get("deleted").asBoolean());

      mirror.commit("sync", checkpoint, Map.of());
      assertEquals(checkpoint, mirror.checkpoint("sync"));
    }
  }

  @Test
  void selectsOnlyTheRequestedCheckpointFromSharedDataFiles() throws Exception {
    try (var mirror = new Mirror(directory.toUri().toString(), Map.of())) {
      var sync = Mirror.JSON.readTree("{\"cursor\":2}");
      var reconcile = Mirror.JSON.readTree("{\"cursor\":7}");
      mirror.commit("sync", sync, Map.of("1", prepared("{\"title\":\"fixture\"}")));
      mirror.commit("reconcile", reconcile, Map.of());
      assertEquals(sync, mirror.checkpoint("sync"));
      assertEquals(reconcile, mirror.checkpoint("reconcile"));
      assertEquals(Mirror.JSON.createObjectNode(), mirror.checkpoint("bootstrap"));
    }
  }

  @Test
  void commitsPreparedRecordsAndTombstonesAtomically() throws Exception {
    String warehouse = directory.toUri().toString();
    try (var mirror = new Mirror(warehouse, Map.of())) {
      var record = prepared("{\"b\":2,\"a\":1}");
      mirror.commit("sync", Mirror.JSON.readTree("{\"cursor\":1}"), Map.of("1", record));
      mirror.commit(
          "sync",
          Mirror.JSON.readTree("{\"cursor\":2}"),
          Map.of("1", prepared("{\"a\":1,\"b\":2}")));
      assertEquals(1, mirror.index().size());
      assertEquals(
          Mirror.JSON.readTree("{\"a\":1,\"b\":2}"),
          mirror.inspect("1", 1, false).get(0).get("payload"));
      assertThrows(
          IllegalArgumentException.class,
          () ->
              mirror.commit(
                  "sync", Mirror.JSON.readTree("{\"cursor\":3}"), Map.of("@invalid", record)));
      assertEquals(2, mirror.checkpoint("sync").path("cursor").asInt());
      mirror.commit(
          "sync",
          Mirror.JSON.readTree("{\"cursor\":3}"),
          Map.of("1", prepared("{\"reason\":\"deleted\"}", true, Mirror.JSON.createObjectNode())));
      mirror.validate();
      mirror.maintain();
      mirror.validate();
    }
    try (var mirror = new Mirror(warehouse, Map.of())) {
      assertTrue(mirror.index().get("1").deleted());
      assertEquals(3, mirror.checkpoint("sync").path("cursor").asInt());
      mirror.commit("sync", Mirror.JSON.createObjectNode(), Map.of("1", prepared("{\"a\":1}")));
      mirror.validate();
    }
  }

  @Test
  void rejectsMalformedControlRowsDuringValidation() throws Exception {
    for (String key : List.of("@sync", "@metadata/100", "@unknown")) {
      try (var mirror =
          new Mirror(
              directory.resolve(key.substring(1).replace('/', '-')).toUri().toString(), Map.of())) {
        var builder = mirror.table.newBatchWriteBuilder();
        try (var write = builder.newWrite();
            var commit = builder.newCommit()) {
          write.write(Mirror.row(key, "", key.equals("@unknown") ? "{}" : "[]", false));
          commit.commit(write.prepareCommit());
        }
        assertThrows(IllegalStateException.class, mirror::validate);
      }
    }
  }

  @Test
  void metadataOnlyCommitsPreserveContent() throws Exception {
    String warehouse = directory.toUri().toString();
    var first =
        Mirror.JSON.readTree(
            "{\"materialized_at\":100,\"parent\":\"tv:1\",\"address\":\"episode:1/0/1\"}");
    var second =
        Mirror.JSON.readTree(
            "{\"materialized_at\":200,\"parent\":\"tv:1\",\"address\":\"episode:1/1/1\"}");
    try (var mirror = new Mirror(warehouse, Map.of())) {
      mirror.commit("sync", first, Map.of("100", prepared("{\"id\":100}", false, first)));
      mirror.commit("sync", second, Map.of("100", new Mirror.Update(null, second)));
      assertEquals(second, mirror.index().get("100").metadata());
      assertEquals(
          Mirror.JSON.readTree("{\"id\":100}"),
          mirror.inspect("100", 1, false).get(0).get("payload"));
    }
    try (var mirror = new Mirror(warehouse, Map.of())) {
      assertEquals(second, mirror.index().get("100").metadata());
      assertEquals(second, mirror.inspect("100", 1, false).get(0).get("metadata"));
      assertEquals(1, mirror.inspect("100", 1, false).size());
      mirror.validate();
    }
  }

  @Test
  void payloadValidationAcceptsNativeJsonValues() {
    for (String payload :
        List.of(
            "null",
            "true",
            "false",
            "42",
            "-1.25e-8",
            "\"escaped\\ntext\\u00e9\"",
            "{}",
            "[]",
            "{\"nested\":[null,true,{\"title\":\"fixture\"}],\"number\":123456789012345678901234567890}")) {
      assertDoesNotThrow(() -> Mirror.validatePayload(payload));
    }
  }

  @Test
  void payloadValidationRejectsMalformedAndMultipleDocuments() {
    for (String payload :
        List.of(
            "",
            " \n",
            "{",
            "{\"nested\":[1,]}",
            "[\"\\q\"]",
            "\"\\uZZZZ\"",
            "\"unterminated",
            "true false",
            "{}[]",
            "{\"id\":1} trailing")) {
      assertThrows(Exception.class, () -> Mirror.validatePayload(payload), payload);
    }
  }

  @Test
  void rejectsInvalidBatchesBeforeAnyWrites() throws Exception {
    try (var mirror = new Mirror(directory.toUri().toString(), Map.of())) {
      var valid = prepared("{\"id\":1}");
      var invalid =
          new Mirror.Update(
              new Mirror.Content("{}", "wrong", false), Mirror.JSON.createObjectNode());
      for (var updates :
          List.of(
              Map.of("1", valid, "2", invalid),
              Map.of("1", valid, "@invalid", valid),
              Map.of("1", valid, "", valid),
              Map.of(
                  "1",
                  valid,
                  "2",
                  new Mirror.Update(valid.content(), Mirror.JSON.createArrayNode())))) {
        assertThrows(
            IllegalArgumentException.class,
            () -> mirror.commit("sync", Mirror.JSON.createObjectNode(), updates));
        assertTrue(mirror.index().isEmpty());
        assertNull(mirror.snapshotId());
      }
      for (String payload : List.of("{\"nested\":[1,]}", "{}[]")) {
        var malformed =
            new Mirror.Update(
                new Mirror.Content(payload, Mirror.hash(payload), false),
                Mirror.JSON.createObjectNode());
        assertThrows(
            Exception.class,
            () ->
                mirror.commit(
                    "sync", Mirror.JSON.createObjectNode(), Map.of("1", valid, "2", malformed)));
        assertTrue(mirror.index().isEmpty());
        assertNull(mirror.snapshotId());
      }
    }
  }
}
