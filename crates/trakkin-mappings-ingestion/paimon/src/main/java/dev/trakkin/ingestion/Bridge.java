package dev.trakkin.ingestion;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.JsonNode;
import java.io.OutputStream;
import java.util.*;

final class Bridge implements AutoCloseable {
  private final Map<String, String> settings;
  private final Map<String, Mirror> warehouses = new LinkedHashMap<>();

  Bridge(Map<String, String> environment) {
    settings = storageSettings(environment);
  }

  void handle(JsonNode request, OutputStream output) throws Exception {
    if (!request.path("version").isIntegralNumber()
        || !request.path("version").canConvertToInt()
        || request.path("version").asInt() != 1) {
      throw new IllegalArgumentException("Expected protocol version 1");
    }
    String warehouse = required(request, "warehouse");
    String action = required(request, "action");
    Map<String, Object> response = new HashMap<>();
    if (action.equals("open")) {
      JsonNode create = request.required("create");
      if (!create.isBoolean()) throw new IllegalArgumentException("Expected boolean create mode");
      if (!warehouses.containsKey(warehouse)) {
        warehouses.put(warehouse, new Mirror(warehouse, settings, create.asBoolean()));
      }
    } else {
      Mirror mirror = warehouses.get(warehouse);
      if (mirror == null) throw new IllegalArgumentException("Warehouse is not open");
      switch (action) {
        case "snapshot" -> response.put("snapshot_id", mirror.snapshotId());
        case "index" -> response.put("entries", mirror.index());
        case "checkpoint" ->
            response.put("checkpoint", mirror.checkpoint(required(request, "operation")));
        case "inspect" ->
            response.put(
                "records",
                mirror.inspect(
                    request.path("key").isTextual() ? request.path("key").asText() : null,
                    request.path("limit").asInt(10),
                    request.path("include_deleted").asBoolean(false)));
        case "commit" -> {
          Map<String, Mirror.Update> updates =
              Mirror.JSON.convertValue(request.required("updates"), new TypeReference<>() {});
          mirror.commit(required(request, "operation"), request.required("checkpoint"), updates);
        }
        case "validate" -> mirror.validate();
        case "maintain" -> mirror.maintain();
        default -> throw new IllegalArgumentException("Unknown storage action");
      }
    }
    respond(response, output);
  }

  static Map<String, String> storageSettings(Map<String, String> environment) {
    Map<String, String> settings = new HashMap<>();
    Map.of(
            "TRAKKIN_MAPPINGS_INGESTION_S3_ENDPOINT", "s3.endpoint",
            "TRAKKIN_MAPPINGS_INGESTION_S3_ACCESS_KEY_ID", "s3.access-key",
            "TRAKKIN_MAPPINGS_INGESTION_S3_SECRET_ACCESS_KEY", "s3.secret-key",
            "TRAKKIN_MAPPINGS_INGESTION_S3_REGION", "s3.endpoint.region",
            "TRAKKIN_MAPPINGS_INGESTION_S3_PATH_STYLE_ACCESS", "s3.path.style.access")
        .forEach(
            (name, option) -> {
              String value = environment.get(name);
              if (value != null && !value.isEmpty()) settings.put(option, value);
            });
    return settings;
  }

  private static String required(JsonNode request, String name) {
    JsonNode value = request.get(name);
    if (value == null || !value.isTextual() || value.asText().isBlank()) {
      throw new IllegalArgumentException("Missing text field " + name);
    }
    return value.asText();
  }

  static void respond(Map<String, Object> response, OutputStream output) throws Exception {
    response.put("version", 1);
    response.put("ok", true);
    try (var generator = Mirror.JSON.getFactory().createGenerator(output)) {
      generator.disable(com.fasterxml.jackson.core.JsonGenerator.Feature.AUTO_CLOSE_TARGET);
      Mirror.JSON.writeValue(generator, response);
      generator.writeRaw('\n');
    }
  }

  @Override
  public void close() throws Exception {
    Exception failure = null;
    for (var mirror : warehouses.values()) {
      try {
        mirror.close();
      } catch (Exception error) {
        if (failure == null) failure = error;
        else failure.addSuppressed(error);
      }
    }
    warehouses.clear();
    if (failure != null) throw failure;
  }
}
