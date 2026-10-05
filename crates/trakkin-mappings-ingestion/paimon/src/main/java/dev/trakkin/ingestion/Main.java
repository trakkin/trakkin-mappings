package dev.trakkin.ingestion;

import com.fasterxml.jackson.databind.JsonNode;
import java.io.*;
import java.util.*;

public final class Main {
    public static void main(String[] args) throws Exception {
        try (var input = new BufferedReader(new InputStreamReader(System.in, java.nio.charset.StandardCharsets.UTF_8))) {
            JsonNode open = Mirror.JSON.readTree(input.readLine());
            if (open.path("version").asInt() != 1 || !open.path("action").asText().equals("open")) {
                throw new IllegalArgumentException("Expected protocol v1 open request");
            }
            Map<String, String> settings = storageSettings(System.getenv());
            try (var mirror = new Mirror(required(open, "warehouse"), settings)) {
                respond(Mirror.JSON.createObjectNode());
                Map<String, Mirror.Entry> index = null;
                String line;
                while ((line = input.readLine()) != null) {
                    JsonNode request = Mirror.JSON.readTree(line);
                    var response = Mirror.JSON.createObjectNode();
                    switch (request.path("action").asText()) {
                        case "snapshot" -> response.set("snapshot_id", Mirror.JSON.valueToTree(mirror.snapshotId()));
                        case "index" -> {
                            index = mirror.index();
                            response.set("entries", Mirror.JSON.valueToTree(index));
                        }
                        case "checkpoint" -> response.set("checkpoint", mirror.checkpoint(required(request, "operation")));
                        case "inspect" -> response.set("records", mirror.inspect(
                            request.path("key").isTextual() ? request.path("key").asText() : null,
                            request.path("limit").asInt(10), request.path("include_deleted").asBoolean(false)));
                        case "commit" -> {
                            if (index == null) index = mirror.index();
                            Map<String, JsonNode> records = new HashMap<>();
                            request.path("records").fields().forEachRemaining(record -> records.put(record.getKey(), record.getValue()));
                            Map<String, JsonNode> deleted = new HashMap<>();
                            request.required("deleted").fields().forEachRemaining(record -> deleted.put(record.getKey(), record.getValue()));
                            Map<String, JsonNode> metadata = new HashMap<>();
                            request.path("metadata").fields().forEachRemaining(record -> metadata.put(record.getKey(), record.getValue()));
                            response.put("changed", mirror.commit(required(request, "operation"), request.required("checkpoint"), records, deleted, metadata, index));
                        }
                        case "validate" -> mirror.validate();
                        case "maintain" -> { mirror.maintain(); index = null; }
                        default -> throw new IllegalArgumentException("Unknown storage action");
                    }
                    respond(response);
                }
            }
        }
    }

    static Map<String, String> storageSettings(Map<String, String> environment) {
        Map<String, String> settings = new HashMap<>();
        Map.of("TRAKKIN_MAPPINGS_INGESTION_S3_ENDPOINT", "s3.endpoint", "TRAKKIN_MAPPINGS_INGESTION_S3_ACCESS_KEY_ID", "s3.access-key",
            "TRAKKIN_MAPPINGS_INGESTION_S3_SECRET_ACCESS_KEY", "s3.secret-key", "TRAKKIN_MAPPINGS_INGESTION_S3_REGION", "s3.endpoint.region",
            "TRAKKIN_MAPPINGS_INGESTION_S3_PATH_STYLE_ACCESS", "s3.path.style.access").forEach((name, option) -> {
                String value = environment.get(name);
                if (value != null && !value.isEmpty()) settings.put(option, value);
            });
        return settings;
    }

    private static String required(JsonNode request, String name) {
        String value = request.required(name).asText();
        if (value.isEmpty()) throw new IllegalArgumentException("Missing " + name);
        return value;
    }

    private static void respond(com.fasterxml.jackson.databind.node.ObjectNode response) throws Exception {
        response.put("version", 1);
        response.put("ok", true);
        System.out.println(Mirror.JSON.writeValueAsString(response));
        System.out.flush();
    }
}