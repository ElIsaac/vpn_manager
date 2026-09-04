#!/bin/bash
# Deshabilita IPv6 de forma persistente (sysctl.conf) y en caliente.
# Se instala en /usr/local/libexec/vpn/ y se invoca con `sudo -n` desde la TUI.

if [ "$EUID" -ne 0 ]; then
  echo "Ejecuta este script como root (sudo)"
  exit 1
fi

SYSCTL_CONF="/etc/sysctl.conf"

echo "Deshabilitando IPv6..."

# Agrega la configuración solo si no existe
grep -q "net.ipv6.conf.all.disable_ipv6" $SYSCTL_CONF || echo "net.ipv6.conf.all.disable_ipv6 = 1" >> $SYSCTL_CONF
grep -q "net.ipv6.conf.default.disable_ipv6" $SYSCTL_CONF || echo "net.ipv6.conf.default.disable_ipv6 = 1" >> $SYSCTL_CONF
grep -q "net.ipv6.conf.lo.disable_ipv6" $SYSCTL_CONF || echo "net.ipv6.conf.lo.disable_ipv6 = 1" >> $SYSCTL_CONF

# Aplica los cambios
sysctl -p

echo "IPv6 ha sido deshabilitado."
