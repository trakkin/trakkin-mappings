package dev.trakkin.ingestion;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;
import java.util.*;
import static org.junit.jupiter.api.Assertions.*;

class MirrorTest {
    @TempDir Path directory;
    @Test void existingOpenDoesNotCreateMissingTable() {
        assertThrows(Exception.class, () -> new Mirror(directory.toUri().toString(), Map.of(), false));
        assertFalse(java.nio.file.Files.exists(directory.resolve("mirrors.db")));
    }

    @Test void existingOpenReadsCreatedTable() throws Exception {
        try (var writer = new Mirror(directory.toUri().toString(), Map.of())) {
            writer.commit("bootstrap", Mirror.JSON.createObjectNode(), Map.of("1", prepared("{\"id\":1}")), Map.of(), writer.index());
            try (var reader = new Mirror(directory.toUri().toString(), Map.of(), false)) {
                assertTrue(reader.index().containsKey("1"));
            }
        }
    }
    @Test void mapsS3EnvironmentToHadoopEndpointRegion() {
        assertEquals(Map.of("s3.endpoint", "https://example.r2.cloudflarestorage.com",
            "s3.endpoint.region", "auto", "s3.path.style.access", "true",
            "s3.access-key", "test-access", "s3.secret-key", "test-secret"), Main.storageSettings(Map.of(
                "TRAKKIN_MAPPINGS_INGESTION_S3_ENDPOINT", "https://example.r2.cloudflarestorage.com",
                "TRAKKIN_MAPPINGS_INGESTION_S3_REGION", "auto",
                "TRAKKIN_MAPPINGS_INGESTION_S3_PATH_STYLE_ACCESS", "true",
                "TRAKKIN_MAPPINGS_INGESTION_S3_ACCESS_KEY_ID", "test-access",
                "TRAKKIN_MAPPINGS_INGESTION_S3_SECRET_ACCESS_KEY", "test-secret")));
        assertEquals(Map.of(), Main.storageSettings(Map.of("TRAKKIN_MAPPINGS_INGESTION_S3_REGION", "")));
    }

    private static com.fasterxml.jackson.databind.JsonNode prepared(String payload) throws Exception {
        String canonical = Mirror.canonical(Mirror.JSON.readTree(payload));
        return Mirror.JSON.createObjectNode().put("payload", canonical).put("hash", Mirror.hash(canonical));
    }
    @Test void selectsOnlyTheRequestedCheckpointFromSharedDataFiles() throws Exception {
        try (var mirror = new Mirror(directory.toUri().toString(), Map.of())) {
            var index = mirror.index();
            var sync = Mirror.JSON.readTree("{\"cursor\":2}");
            var reconcile = Mirror.JSON.readTree("{\"cursor\":7}");
            mirror.commit("sync", sync, Map.of("1", prepared("{\"title\":\"fixture\"}")), Map.of(), index);
            mirror.commit("reconcile", reconcile, Map.of(), Map.of(), index);
            assertEquals(sync, mirror.checkpoint("sync"));
            assertEquals(reconcile, mirror.checkpoint("reconcile"));
            assertEquals(Mirror.JSON.createObjectNode(), mirror.checkpoint("bootstrap"));
        }
    }

    @Test void commitsCheckpointWithIdempotentRecordsAndTombstones() throws Exception {
        String warehouse = directory.toUri().toString();
        try (var mirror = new Mirror(warehouse, Map.of())) {
            var index = mirror.index();
            var record = prepared("{\"b\":2,\"a\":1}");
            assertEquals(1, mirror.commit("sync", Mirror.JSON.readTree("{\"cursor\":1}"), Map.of("1", record), Map.of(), index));
            assertEquals(0, mirror.commit("sync", Mirror.JSON.readTree("{\"cursor\":2}"), Map.of("1", prepared("{\"a\":1,\"b\":2}")), Map.of(), index));
            assertThrows(IllegalArgumentException.class, () -> mirror.commit("sync", Mirror.JSON.readTree("{\"cursor\":3}"), Map.of("@invalid", record), Map.of(), index));
            assertEquals(2, mirror.checkpoint("sync").path("cursor").asInt());
            assertEquals(1, mirror.commit("sync", Mirror.JSON.readTree("{\"cursor\":3}"), Map.of(), Map.of("1", prepared("{\"reason\":\"deleted\"}")), index));
            mirror.validate();
            mirror.maintain();
            mirror.validate();
        }
        try (var mirror = new Mirror(warehouse, Map.of())) {
            assertTrue(mirror.index().get("1").deleted());
            assertEquals(3, mirror.checkpoint("sync").path("cursor").asInt());
            assertEquals(1, mirror.commit("sync", Mirror.JSON.createObjectNode(), Map.of("1", prepared("{\"a\":1}")), Map.of(), mirror.index()));
            mirror.validate();
        }
    }

    @Test void rejectsMalformedControlRowsDuringValidation() throws Exception {
        for (String key : List.of("@sync", "@metadata/100", "@unknown")) {
            try (var mirror = new Mirror(directory.resolve(key.substring(1).replace('/', '-')).toUri().toString(), Map.of())) {
                var builder = mirror.table.newBatchWriteBuilder();
                try (var write = builder.newWrite(); var commit = builder.newCommit()) {
                    write.write(Mirror.row(key, "", key.equals("@unknown") ? "{}" : "[]", false));
                    commit.commit(write.prepareCommit());
                }
                assertThrows(IllegalStateException.class, mirror::validate);
            }
        }
    }

    @Test void metadataIsAtomicAndDoesNotRewriteUnchangedContent() throws Exception {
        String warehouse = directory.toUri().toString();
        var first = Mirror.JSON.readTree("{\"materialized_at\":100,\"parent\":\"tv:1\",\"address\":\"episode:1/0/1\"}");
        var second = Mirror.JSON.readTree("{\"materialized_at\":200,\"parent\":\"tv:1\",\"address\":\"episode:1/1/1\"}");
        try (var mirror = new Mirror(warehouse, Map.of())) {
            var index = mirror.index();
            assertEquals(1, mirror.commit("sync", first, Map.of("100", prepared("{\"id\":100}")), Map.of(), Map.of("100", first), index));
            assertEquals(0, mirror.commit("sync", second, Map.of(), Map.of(), Map.of("100", second), index));
            assertEquals(second, index.get("100").metadata());
        }
        try (var mirror = new Mirror(warehouse, Map.of())) {
            assertEquals(second, mirror.index().get("100").metadata());
            assertEquals(second, mirror.inspect("100", 1, false).get(0).get("metadata"));
            assertEquals(1, mirror.inspect("100", 1, false).size());
            mirror.validate();
        }
    }
}