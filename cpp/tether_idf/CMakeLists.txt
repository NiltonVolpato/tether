# tether's glue for ESP-IDF: a tether::Server's I/O over a UART, from FreeRTOS
# tasks. The core (../tether) knows nothing of the platform; this is all of it.

idf_component_register(
  SRCS src/uart_io.cpp
  INCLUDE_DIRS include
  REQUIRES tether esp_driver_uart freertos
  PRIV_REQUIRES esp_timer)
