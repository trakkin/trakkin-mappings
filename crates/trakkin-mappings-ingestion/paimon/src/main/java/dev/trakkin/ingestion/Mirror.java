package dev.trakkin.ingestion;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.apache.paimon.catalog.*;
import org.apache.paimon.data.*;
import org.apache.paimon.options.Options;
import org.apache.paimon.schema.Schema;
import org.apache.paimon.table.Table;
import org.apache.paimon.types.DataTypes;
import java.security.MessageDigest;
import java.util.*;

public final class Mirror implements AutoCloseable {
    static final ObjectMapper JSON = new ObjectMapper();
    private final Catalog catalog;
    Table table;
    private final Identifier identifier;
    public record Entry(String hash, boolean deleted, JsonNode metadata) {
        public Entry(String hash, boolean deleted) { this(hash, deleted, JSON.createObjectNode()); }
    }

    public Mirror(String warehouse, Map<String, String> settings) throws Exception {
        this(warehouse, settings, true);
    }

    public Mirror(String warehouse, Map<String, String> settings, boolean create) throws Exception {
        java.net.InetAddress.getAllByName("localhost");
        Options options = new Options();
        settings.forEach(options::set);
        options.set("warehouse", warehouse);
        catalog = CatalogFactory.createCatalog(CatalogContext.create(options));
        identifier = Identifier.create("mirrors", "records");
        if (create) {
            catalog.createDatabase("mirrors", true);
            catalog.createTable(identifier, Schema.newBuilder()
                .column("key", DataTypes.STRING().notNull())
                .column("hash", DataTypes.STRING())
                .column("payload", DataTypes.STRING())
                .column("deleted", DataTypes.BOOLEAN())
                .primaryKey("key").option("bucket", "1")
                .option("snapshot.time-retained", "7 d")
                .option("snapshot.num-retained.min", "2")
                .option("snapshot.num-retained.max", "1000").build(), true);
        }
        table = catalog.getTable(identifier);
    }

    public Map<String, Entry> index() throws Exception {
        catalog.invalidateTable(identifier);
        table = catalog.getTable(identifier);
        Map<String, Entry> entries = new HashMap<>();
        Map<String, JsonNode> metadata = new HashMap<>();
        var read = table.newReadBuilder();
        try (var reader = read.newRead().createReader(read.newScan().plan().splits())) {
            reader.forEachRemaining(row -> {
                String key = row.getString(0).toString();
                if (key.startsWith("@metadata/")) {
                    try { metadata.put(key.substring(10), JSON.readTree(row.getString(2).toString())); }
                    catch (Exception failure) { throw new IllegalStateException("Invalid metadata", failure); }
                } else if (!key.startsWith("@")) entries.put(key, new Entry(row.getString(1).toString(), row.getBoolean(3)));
            });
        }
        entries.replaceAll((key, entry) -> new Entry(entry.hash(), entry.deleted(), metadata.getOrDefault(key, JSON.createObjectNode())));
        return entries;
    }

    public Long snapshotId() throws Exception {
        catalog.invalidateTable(identifier);
        return ((org.apache.paimon.table.FileStoreTable) catalog.getTable(identifier)).snapshotManager().latestSnapshotId();
    }

    public JsonNode checkpoint(String operation) throws Exception {
        catalog.invalidateTable(identifier);
        table = catalog.getTable(identifier);
        var predicate = new org.apache.paimon.predicate.PredicateBuilder(table.rowType());
        var read = table.newReadBuilder().withFilter(predicate.equal(0, BinaryString.fromString("@" + operation)));
        List<String> values = new ArrayList<>();
        try (var reader = read.newRead().createReader(read.newScan().plan().splits())) {
            reader.forEachRemaining(row -> {
                if (row.getString(0).toString().equals("@" + operation)) {
                    values.add(row.getString(2).toString());
                }
            });
        }
        return values.isEmpty() ? JSON.createObjectNode() : JSON.readTree(values.getFirst());
    }

    public JsonNode inspect(String requestedKey, int limit, boolean includeDeleted) throws Exception {
        if (limit < 1 || limit > 1000) throw new IllegalArgumentException("Inspection limit must be between 1 and 1000");
        if (requestedKey != null && requestedKey.startsWith("@")) throw new IllegalArgumentException("Reserved record key");
        catalog.invalidateTable(identifier);
        table = catalog.getTable(identifier);
        var read = table.newReadBuilder();
        if (requestedKey != null) {
            var predicate = new org.apache.paimon.predicate.PredicateBuilder(table.rowType());
            read.withFilter(predicate.equal(0, BinaryString.fromString(requestedKey)));
        }
        var records = JSON.createArrayNode();
        try (var reader = read.newRead().createReader(read.newScan().plan().splits())) {
            while (records.size() < limit) {
                var batch = reader.readBatch();
                if (batch == null) break;
                try {
                    InternalRow row;
                    while (records.size() < limit && (row = batch.next()) != null) {
                        String key = row.getString(0).toString();
                        if (key.startsWith("@") || (requestedKey != null && !key.equals(requestedKey))) continue;
                        if (requestedKey == null && row.getBoolean(3) && !includeDeleted) continue;
                        var record = JSON.createObjectNode().put("key", key)
                            .put("hash", row.getString(1).toString()).put("deleted", row.getBoolean(3));
                        record.set("payload", JSON.readTree(row.getString(2).toString()));
                        record.set("metadata", checkpoint("metadata/" + key));
                        records.add(record);
                    }
                } finally { batch.releaseBatch(); }
            }
        }
        return records;
    }

    public int commit(String operation, JsonNode checkpoint, Map<String, JsonNode> records,
                      Map<String, JsonNode> deleted, Map<String, Entry> index) throws Exception {
        return commit(operation, checkpoint, records, deleted, Map.of(), index);
    }

    public int commit(String operation, JsonNode checkpoint, Map<String, JsonNode> records,
                      Map<String, JsonNode> deleted, Map<String, JsonNode> metadata, Map<String, Entry> index) throws Exception {
        Map<String, Entry> changes = new HashMap<>();
        var builder = table.newBatchWriteBuilder();
        try (var write = builder.newWrite(); var commit = builder.newCommit()) {
            for (var record : records.entrySet()) {
                String key = record.getKey();
                if (key.startsWith("@")) throw new IllegalArgumentException("Reserved record key");
                String payload = record.getValue().required("payload").asText();
                Entry entry = new Entry(record.getValue().required("hash").asText(), false);
                if (index.get(key) == null || !entry.hash().equals(index.get(key).hash()) || index.get(key).deleted()) {
                    write.write(row(key, entry.hash(), payload, false));
                    changes.put(key, entry);
                }
            }
            for (var deletion : deleted.entrySet()) {
                String key = deletion.getKey();
                if (key.startsWith("@") || records.containsKey(key)) throw new IllegalArgumentException("Conflicting deletion");
                String payload = deletion.getValue().required("payload").asText();
                Entry entry = new Entry(deletion.getValue().required("hash").asText(), true);
                if (index.get(key) == null || !entry.hash().equals(index.get(key).hash()) || !index.get(key).deleted()) {
                    write.write(row(key, entry.hash(), payload, true));
                    changes.put(key, entry);
                }
            }
            for (var observation : metadata.entrySet()) {
                if (observation.getKey().startsWith("@")) throw new IllegalArgumentException("Reserved metadata key");
                var existing = index.get(observation.getKey());
                if (existing == null || !existing.metadata().equals(observation.getValue())) {
                    write.write(row("@metadata/" + observation.getKey(), "", canonical(observation.getValue()), false));
                }
            }
            write.write(row("@" + operation, "", canonical(checkpoint), false));
            commit.commit(write.prepareCommit());
        }
        index.putAll(changes);
        metadata.forEach((key, value) -> index.computeIfPresent(key, (ignored, entry) -> new Entry(entry.hash(), entry.deleted(), value)));
        return changes.size();
    }

    static GenericRow row(String key, String hash, String payload, boolean deleted) {
        return GenericRow.of(BinaryString.fromString(key), BinaryString.fromString(hash), BinaryString.fromString(payload), deleted);
    }

    static String canonical(JsonNode node) throws Exception {
        if (node.isObject()) {
            var sorted = JSON.createObjectNode();
            var names = new TreeSet<String>();
            node.fieldNames().forEachRemaining(names::add);
            for (String name : names) sorted.set(name, JSON.readTree(canonical(node.get(name))));
            return JSON.writeValueAsString(sorted);
        }
        if (node.isArray()) {
            var array = JSON.createArrayNode();
            for (JsonNode child : node) array.add(JSON.readTree(canonical(child)));
            return JSON.writeValueAsString(array);
        }
        return JSON.writeValueAsString(node);
    }

    static String hash(String payload) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(payload.getBytes(java.nio.charset.StandardCharsets.UTF_8)));
    }

    public void validate() throws Exception {
        catalog.invalidateTable(identifier);
        table = catalog.getTable(identifier);
        var read = table.newReadBuilder();
        try (var reader = read.newRead().createReader(read.newScan().plan().splits())) {
            reader.forEachRemaining(row -> {
                String key = row.getString(0).toString();
                try {
                    String payload = row.getString(2).toString();
                    JsonNode value = JSON.readTree(payload);
                    if (value == null) throw new IllegalStateException("Missing JSON payload");
                    if (key.startsWith("@")) {
                        if (!value.isObject()) throw new IllegalStateException("Control row must be an object");
                        if (!key.equals("@bootstrap") && !key.equals("@sync") && !key.equals("@reconcile")
                            && !(key.startsWith("@metadata/") && key.length() > 10 && !key.substring(10).startsWith("@"))) {
                            throw new IllegalStateException("Unknown reserved key");
                        }
                        if (row.getBoolean(3)) throw new IllegalStateException("Deleted control row");
                    } else {
                        if (!hash(payload).equals(row.getString(1).toString())) throw new IllegalStateException("Hash mismatch: " + key);
                    }
                } catch (Exception failure) { throw new IllegalStateException("Invalid record: " + key, failure); }
            });
        }
    }

    public void close() throws Exception { catalog.close(); }

    public void maintain() throws Exception {
        catalog.invalidateTable(identifier);
        var current = (org.apache.paimon.table.FileStoreTable) catalog.getTable(identifier);
        var builder = current.newBatchWriteBuilder();
        try (var write = builder.newWrite(); var commit = builder.newCommit()) {
            write.compact(BinaryRow.EMPTY_ROW, 0, true);
            commit.commit(write.prepareCommit());
        }
        catalog.invalidateTable(identifier);
        current = (org.apache.paimon.table.FileStoreTable) catalog.getTable(identifier);
        try (var commit = current.newCommit(UUID.randomUUID().toString())) {
            commit.compactManifests();
            commit.expireSnapshots();
        }
        new org.apache.paimon.operation.LocalOrphanFilesClean(current,
            System.currentTimeMillis() - java.time.Duration.ofDays(7).toMillis(), false).clean();
    }
}