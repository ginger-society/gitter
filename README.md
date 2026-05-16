ssh-keygen -R "[localhost]:8022"

ssh -i admin_key -p 8022 git@localhost info

ssh -i user_key -p 8022 git@localhost


GIT_SSH_COMMAND="ssh -i ./admin_key -o StrictHostKeyChecking=no -p 8022" git clone ssh://git@localhost/gitolite-admin




env:
  - name: KUBECONFIG_DATA
    valueFrom:
      secretKeyRef:
        name: $(params.deployment_target_secret)  # "deployment-target-dev-alice"
        key: kubeconfig.yaml