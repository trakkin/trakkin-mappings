package dev.trakkin.ingestion;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;

public final class Main {
  public static void main(String[] args) throws Exception {
    try (var input = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        var bridge = new Bridge(System.getenv())) {
      String line;
      while ((line = input.readLine()) != null) {
        bridge.handle(Mirror.JSON.readTree(line), System.out);
      }
    }
  }
}
