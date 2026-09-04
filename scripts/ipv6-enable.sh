#!/bin/bash
# Rehabilita IPv6: quita las líneas de sysctl.conf y lo aplica en caliente.
# Se instala en /usr/local/libexec/vpn/ y se invoca con `sudo -n` desde la TUI.

if [ "$EUID" -ne 0 ]; then
  echo "Ejecuta este script como root (sudo)"
  exit 1
fi

SYSCTL_CONF="/etc/sysctl.conf"

echo "Habilitando IPv6..."

# Quita las líneas que deshabilitan IPv6
sed -i '/net.ipv6.conf.all.disable_ipv6/d' $SYSCTL_CONF
sed -i '/net.ipv6.conf.default.disable_ipv6/d' $SYSCTL_CONF
sed -i '/net.ipv6.conf.lo.disable_ipv6/d' $SYSCTL_CONF

# Aplica el cambio en caliente (sin reboot)
sysctl -w net.ipv6.conf.all.disable_ipv6=0
sysctl -w net.ipv6.conf.default.disable_ipv6=0
sysctl -w net.ipv6.conf.lo.disable_ipv6=0

# Recarga el sysctl.conf para validar
sysctl -p

echo "IPv6 ha sido habilitado."
