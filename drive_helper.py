import os
import sys
import io
import json
from google.oauth2 import service_account
from googleapiclient.discovery import build
from googleapiclient.http import MediaIoBaseDownload, MediaFileUpload

KEY_FILE = "/home/ubuntu/.hermes/google_service_account.json"
SCOPES = ["https://www.googleapis.com/auth/drive"]

def get_drive_service():
    creds = service_account.Credentials.from_service_account_file(KEY_FILE, scopes=SCOPES)
    return build("drive", "v3", credentials=creds)

def download_file(file_id, output_path):
    service = get_drive_service()
    request = service.files().get_media(fileId=file_id)
    fh = io.FileIO(output_path, 'wb')
    downloader = MediaIoBaseDownload(fh, request)
    done = False
    print(f"Downloading {file_id} to {output_path}...")
    while done is False:
        status, done = downloader.next_chunk()
        print(f"Download Progress: {int(status.progress() * 100)}%")

def upload_file(file_path, name, parent_id=None):
    service = get_drive_service()
    file_metadata = {'name': name}
    if parent_id:
        file_metadata['parents'] = [parent_id]
    
    media = MediaFileUpload(file_path, resumable=True)
    file = service.files().create(body=file_metadata, media_body=media, fields='id, webViewLink').execute()
    print(f"File uploaded. ID: {file.get('id')}, Link: {file.get('webViewLink')}")
    return file

def update_file(file_id, file_path):
    service = get_drive_service()
    media = MediaFileUpload(file_path, resumable=True)
    file = service.files().update(
        fileId=file_id,
        media_body=media,
        fields="id,name,size,webViewLink",
    ).execute()
    print(f"File updated. ID: {file.get('id')}, Link: {file.get('webViewLink')}")
    return file

if __name__ == "__main__":
    action = sys.argv[1]
    if action == "download":
        download_file(sys.argv[2], sys.argv[3])
    elif action == "upload":
        parent = sys.argv[4] if len(sys.argv) > 4 else None
        upload_file(sys.argv[2], sys.argv[3], parent)
    elif action == "update":
        update_file(sys.argv[2], sys.argv[3])
