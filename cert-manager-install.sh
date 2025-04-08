#!/bin/bash

# Define as variáveis
CLUSTER_NAME="oci-eita"
EMAIL="svc@eita.cloud"
NAMESPACE="cert-manager"
LETS_ENCRYPT_ISSUER="letsencrypt-issuer"

# Função para instalar o Helm
install_helm() {
  curl -fsSL https://raw.githubusercontent.com/helm/helm/master/scripts/get-helm-3.sh | bash
}

# Função para instalar o Cert Manager
install_cert_manager() {
  helm repo add jetstack https://charts.jetstack.io
  helm repo update
  helm install cert-manager jetstack/cert-manager \
    --namespace $NAMESPACE \
    --set installCRDs=true \
    --set "extraArgs={--issuer=$LETS_ENCRYPT_ISSUER}"
}

# Função para configurar o Let's Encrypt issuer
configure_lets_encrypt_issuer() {
  cat <<EOF | kubectl apply -f -
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: $LETS_ENCRYPT_ISSUER
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: $EMAIL
    privateKeySecretRef:
      name: $LETS_ENCRYPT_ISSUER
    solvers:
    - http:
        ingress:
          class: traefik
EOF
}

# Main
install_helm
install_cert_manager
configure_lets_encrypt_issuer