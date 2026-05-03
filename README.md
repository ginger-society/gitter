ssh-keygen -R "[localhost]:8022"

ssh -i admin_key -p 8022 git@localhost info

ssh -i user_key -p 8022 git@localhost